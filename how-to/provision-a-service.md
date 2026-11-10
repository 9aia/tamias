# How to provision and service-manage a package

`install` puts a payload in the store. **Provisioning** (materializing a
runnable artifact), **deploy** (activation), and the **service unit** are
the driver's job — tamias shells the driver and never learns its schema.

## Declare the provider in the manifest

A package can carry a declarative `provider:` block in `tamias.yaml` —
the spec a deploy driver materializes (service units, ingress/forge
hooks, a health probe). tamias validates it (`tamias schema` emits the
JSON Schema) but never interprets it; the driver's `deploy` verb turns
it into host resources.

```yaml
name: ci-gate
version: 1.0.0
type: peregrin-bundle
provider:
  name: ci-gate                  # defaults to the package name
  services:
    - name: ci-gate              # unit name ([A-Za-z0-9_.@-])
      exec: "peregrin campaign serve /srv/ci"   # argv string, quoted by the driver
      env_file: ~/.config/peregrin/env         # EnvironmentFile=
      env: {PORT: "29173"}                     # inline env — secrets stay in env_file/keyring
      workdir: "~"                             # quote a bare ~ (YAML null otherwise)
      restart: on-failure                     # Restart=, default on-failure
  ingress:
    - public_url: https://ci.example.com
      listen: 127.0.0.1:29173
      path: /hook
      hooks:                   # declarative forge hooks for driver deploy
        - forge: github
          repo: 9aia/peregrin
          events: [push]
  verbs: [run, logs, health]   # advisory CLI hints
  health: {cmd: "curl -sf localhost:29173/health"}
```

Every field is optional; no `provider:` block means nothing to deploy —
today's behavior. `Manifest::validate` rejects malformed specs (a
service without a name or `exec`, duplicate unit names, line breaks in
single-line fields, hooks missing `forge`/`repo`).

## Deploy an installed package

```sh
tamias install git:9aia/peregrin//libraries/ci-gate#main
tamias deploy ci-gate                       # runs the driver's deploy step
tamias deploy ci-gate --config ./node.yaml   # operator params ride a config file
```

A package that ships its own `drivers/` needs `--allow-drivers` at
install time (or `TAMIAS_ALLOW_DRIVERS=1`) — the consent gate is
described in [Security model](/explanation/security).

If the package's type has no driver, `deploy` reports
`nothing to deploy (type …)` and succeeds. `uninstall` runs the driver's
teardown first; with no driver it warns that provider-side resources may
remain, then drops the store entry.

## Manage the service unit

The per-user **systemd** mechanism:

```sh
tamias service install --unit ci-gate \
  --exec "/usr/local/bin/ci-gate run" \
  --description "ci-gate" \
  --env HOME=/home/me \
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
tamias uninstall ci-gate    # driver teardown + drop the store entry
```

## See also

- [Write a driver](/how-to/write-a-driver)
- [Content types](/explanation/content-types)
