# How to provision and service-manage a package

`install` puts a payload in the store. **Deploy** has two halves:

- the package's **kind driver** (`tamias-driver-<type> deploy`) —
  kind-specific setup/teardown tamias never interprets;
- the manifest's declarative **`provider:` block** — tamias-native
  service/runtime spec that `tamias deploy` materializes itself.

`provider.services[]` renders one **systemd user unit** per entry — no
driver needed, for any content type (including `files`). launchd is
documented but not yet implemented: the local deploy adapter is
systemd-only today.

## Declare the provider in the manifest

```yaml
name: ci-gate
version: 1.0.0
type: peregrin-bundle
provider:
  name: ci-gate                  # unit-name prefix; defaults to the package name
  services:
    - name: ci-gate              # unit suffix → unit <provider>-<service>.service
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

### Unit rendering

Each `provider.services[]` entry becomes `<provider.name-or-package>-<service>.service`
under `~/.config/systemd/user` (`$XDG_CONFIG_HOME/systemd/user`):

- `After=`/`Wants=network-online.target`;
- `Type=simple`, `Restart=` (default `on-failure`), `RestartSec=5`;
- `EnvironmentFile=-<env_file>` — the `-` makes a missing file
  non-fatal;
- `Environment=K=V` per `env` entry, `WorkingDirectory=` per `workdir`;
- `~`/`~/…` in `workdir` and `env_file` expands to `$HOME` at render
  time — **quote it in YAML** (`workdir: "~"`); a bare `~` parses as
  null.

Unit names are deterministic: `provider.name` wins, else the package
name (scope-qualified — `@ci/gate` → `ci-gate-…`). Two packages claiming
the same unit name resolve last-deploy-wins.

## Deploy an installed package

```sh
tamias install git:9aia/peregrin//libraries/ci-gate#main
tamias deploy ci-gate                       # driver deploy + render provider.services units
tamias deploy ci-gate --config ./node.yaml   # operator params ride a config file
```

`deploy` runs the kind driver's `deploy` verb *and* writes every
`provider.services[]` unit (then `systemctl --user daemon-reload`,
best-effort). Units are **not enabled** — activation stays explicit:

```sh
tamias service enable ci-gate-ci-gate
tamias service status ci-gate-ci-gate
tamias service logs   ci-gate-ci-gate
```

A package that ships its own `drivers/` needs `--allow-drivers` at
install time (or `TAMIAS_ALLOW_DRIVERS=1`) — the consent gate is
described in [Security model](/explanation/security).

If neither a driver nor `provider.services[]` applies, `deploy` reports
`nothing to deploy (type …)` and succeeds. `uninstall` runs the driver's
teardown, removes the declarative units (best-effort per unit), then
drops the store entry; with no driver it warns that provider-side
resources may remain.

## Manage an ad-hoc service unit

The per-user **systemd** mechanism also works without a package:

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

- [Write a driver](/how-to/write-a-driver)
- [Content types](/explanation/content-types)
