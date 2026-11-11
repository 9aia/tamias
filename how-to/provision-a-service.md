---
url: /tamias/how-to/provision-a-service.md
---
# How to provision and service-manage a package

`install` puts a payload in the store. **Deploy** has two halves:

* the package's **kind driver** (`tamias-driver-<type> deploy`) —
  kind-specific setup/teardown tamias never interprets;
* the manifest's declarative **`provider:` block** — tamias-native
  service/runtime spec that `tamias deploy` materializes itself.

`provider.services[]` renders one **per-user service** per entry — no
driver needed, for any content type (including `files`). The backend
is picked by platform (`tamias_provider::ServiceBackend`): **systemd
user units** on Linux, **launchd LaunchAgents** on macOS
(`~/Library/LaunchAgents`, `gui/<uid>` domain), and **Task Scheduler
tasks** on Windows (`schtasks`, `%LOCALAPPDATA%\tamias\tasks`). All
three render the same `ServiceSpec`; each backend documents its honest
gaps (e.g. Task Scheduler caps restart count, launchd has no
`Description=`/env-file keys — a wrapper script covers `env_file`).

## Declare the provider in the manifest

```yaml
name: ci-gate
version: 1.0.0
type: peregrin-bundle
provider:
  name: ci-gate                  # unit-name prefix; defaults to the package name
  services:
    - name: ci-gate              # service suffix → <provider>-<service>
                                 # (.service on systemd; label/task elsewhere)
      exec: "peregrin campaign serve /srv/ci --port '2917 3'"  # argv string
      env_file: "~/.config/peregrin/env"   # EnvironmentFile=- (optional)
      env: {PORT: "29173"}                 # inline env — secrets stay in env_file/keyring
      workdir: "~"                         # quote a bare ~ (YAML null otherwise)
      restart: on-failure                 # Restart=, default on-failure
  ingress:
    - public_url: https://ci.example.com
      listen: 127.0.0.1:29173
      path: /hook
      hooks:                   # declarative forge hooks — driver deploy consumes these
        - forge: github
          repo: 9aia/peregrin
          events: [push]
  verbs: [run, logs, health]   # advisory CLI hints
  health: {cmd: "curl -sf localhost:29173/health"}
```

Every field is optional; no `provider:` block means nothing to deploy —
today's behavior. `Manifest::validate` rejects malformed specs (a
service without a name or `exec`, duplicate service names, line breaks
in single-line fields, hooks missing `forge`/`repo`).

### What `exec` means

`exec` is a **command line split into argv** — not a shell command.
`tamias` splits it with a small POSIX-ish subset: whitespace separates
arguments, `'…'` is literal, `"…"` allows `\"`/`\\`, and `\c` escapes any
character outside quotes. `$VAR`, globs, pipes, and `#` comments are
*not* expanded — the string is data. Each argv element is then quoted
for `ExecStart=` (systemd double-quote rules), so an argument with
spaces survives intact:

```yaml
exec: "/usr/bin/gate serve --name 'two words' --flag=\"a b\""
# → ExecStart=/usr/bin/gate serve --name "two words" --flag="a b"
```

An unterminated quote or an empty argv fails the deploy.

### Unit rendering (systemd — the Linux backend)

On Linux each `provider.services[]` entry becomes
`<provider.name-or-package>-<service>.service` under
`~/.config/systemd/user` (`$XDG_CONFIG_HOME/systemd/user`). The macOS
and Windows backends render the same fields into a launchd plist
(+ optional `.sh` env-file wrapper) and a Task Scheduler `.cmd`/`.xml`
pair respectively — see each `tamias-deploy-*` crate's docs for the
per-field mapping.

* `After=`/`Wants=network-online.target`;
* `Type=simple`, `Restart=` (default `on-failure`), `RestartSec=5`;
* `EnvironmentFile=-<env_file>` — the `-` makes a missing file
  non-fatal;
* `Environment=K=V` per `env` entry, `WorkingDirectory=` per `workdir`;
* `~`/`~/…` in `workdir` and `env_file` expands to `$HOME` at render
  time — **quote it in YAML** (`workdir: "~"`); a bare `~` parses as
  null.

Unit names are deterministic: `provider.name` wins, else the package
name (scope-qualified — `@ci/gate` → `ci-gate-…`). Two packages claiming
the same unit name resolve last-deploy-wins.

A shared unit name is a feature, not just a collision: packages that
declare the same `provider.name` + service name render **one** unit —
the canonical shape is several content bundles joining a single host
process (e.g. `peregrin-bundle`s whose `campaign` service runs
`campaign serve <dir>` over a directory of generated configs — one
bundle's deploy drops its config there, the host serves them all).
Keep `exec` (and `env_file`/`workdir`) identical across declarers —
last deploy wins the unit file. `uninstall` re-renders a still-shared
unit from a surviving declarer instead of dropping it; only the last
declarer's removal removes the unit.

## Deploy an installed package

```sh
tamias install git:9aia/bupha//libraries/ci-gate#main
tamias deploy ci-gate                       # driver deploy + render provider.services units
tamias deploy ci-gate --config ./node.yaml   # operator params ride a config file
```

`deploy` runs the kind driver's `deploy` verb *and* writes every
`provider.services[]` service through the platform backend (then a
best-effort reload — `systemctl --user daemon-reload` on systemd, a
no-op elsewhere). Services are **not enabled** — activation stays
explicit:

```sh
tamias service enable ci-gate-ci-gate   # systemd: enable --now; launchd:
                                        # bootstrap; Windows: schtasks /change
tamias service status ci-gate-ci-gate    # per-backend status probe
tamias service logs   ci-gate-ci-gate   # journalctl / launchd log tail /
                                        # schtasks snapshot, per backend
```

A package that ships its own `drivers/` needs `--allow-drivers` at
install time (or `TAMIAS_ALLOW_DRIVERS=1`) — the consent gate is
described in [Security model](/explanation/security).

If neither a driver nor `provider.services[]` applies, `deploy` reports
`nothing to deploy (type …)` and succeeds. `uninstall` runs the driver's
teardown, removes the declarative units (best-effort per unit — a unit
another installed provider still declares is re-rendered from the
survivor instead), then drops the store entry; with no driver it warns
that provider-side resources may remain. When the manifest declares `ingress[].hooks[]`,
`deploy` prints a hint pointing at `tamias driver exec <pkg> hooks
register --dry-run` — hook registration stays explicit (the
`public_url` fronts a tunnel the operator confirms first).

## Run provider verbs through the driver

The lifecycle verbs (`install`/`deploy`/`uninstall`) aren't the whole
surface — a driver may declare extra ops verbs in `capabilities.verbs`
(`hooks`, `health`, `status`, …). `tamias driver exec` is the generic
passthrough:

```sh
tamias drivers --json                        # each driver's declared verbs
tamias driver exec ci-gate hooks register --dry-run
tamias driver exec ci-gate hooks register    # real run — forge API + secrets
tamias driver exec ci-gate hooks unregister
tamias driver exec ci-gate health            # JSON status probe
tamias driver exec ci-gate status            # per-unit is-active/is-enabled
tamias driver exec ci-gate logs --lines 50
```

The verb plus everything after it passes through verbatim after the
standard `--dir`/`--name` flags (add `--config <file>` before the verb's
args to forward a node config). The driver's stdout/stderr stream back.
`tamias run` stays the *local* service-exec surface — provider-specific
ops that need the driver's context (manifest secrets, forge API, unit
state) go through `driver exec` instead. `tamias deploy` never
auto-registers ingress hooks: `public_url` fronts a tunnel the operator
confirms, so registration is always an explicit, reviewable step.

## Inspect providers

```sh
tamias provider list          # installed packages' provider: blocks
                              # (services + unit state, ingress, verbs)
tamias provider list --all    # include packages without a provider:
tamias provider list --json   # machine-readable
```

Each service row shows its rendered unit name (`<provider>-<service>`
— `.service` on systemd, a launchd label or task name elsewhere) and a
best-effort enable-state probe (`-` when the backend can't answer).
Ingress rows carry the `public_url` (+ `listen`/`path`).

## Run a service without a service manager

`tamias run` execs a provider service's `exec` argv **in the foreground** —
the same argv/env the rendered unit would get, minus the backend:

```sh
tamias run ci-gate            # sole service → runs it
tamias run ci-gate web        # pick one of several
tamias run ci-gate web -- --port 29174   # extra argv after `--`
```

The child gets a scrubbed environment (parent credentials never leak —
see [Security model](/explanation/security)): the service's `env` plus
its `env_file` parsed as `K=V` lines (inline `env` wins, matching
systemd's `EnvironmentFile` → `Environment=` order). `workdir` (`~`
expands) is the cwd, else the payload dir. A non-zero exit fails the
command. This is the dev/no-service-manager counterpart of
`service enable` — `provider.health.cmd` probes and richer verbs are
still on the roadmap.

## Follow a service's logs

```sh
tamias logs ci-gate            # ALL the package's units, one stream
                               # (journalctl / launchd log tail / schtasks)
tamias logs ci-gate --lines 50 -f
tamias logs ci-gate-ci-gate    # or a bare unit name
tamias service logs ci-gate-ci-gate   # unit-level, same backend stream
```

A package name resolves to every `provider.services[]` unit in one
stream; anything that isn't an installed package is treated as a unit
base name.

## Manage an ad-hoc service

The per-user service mechanism also works without a package — same
verbs, same platform backend:

```sh
tamias service install --unit ci-gate \
  --exec "/usr/local/bin/ci-gate run --flag 'a b'" \
  --env-file ~/.config/ci-gate/env \
  --description "ci-gate" \
  --env HOME=/home/me \
  --restart on-failure \
  --workdir /srv/ci-gate
tamias service enable  ci-gate
tamias service status  ci-gate
tamias service logs    ci-gate
tamias service disable ci-gate
tamias service uninstall ci-gate
```

`service install` writes the unit (it does **not** enable it). `uninstall`
disables + removes the unit file.

## Tear down

```sh
tamias uninstall ci-gate    # driver teardown + unit removal + drop the store entry
```

## See also

* [Write a driver](/how-to/write-a-driver)
* [Content types](/explanation/content-types)
