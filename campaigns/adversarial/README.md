# campaigns/adversarial — attack → triage → fix → verify

A [peregrin](https://github.com/9aia/peregrin) campaign that makes
tamias eat its own cooking: an **attack** node runs the adversarial
battery, **triage** registers every finding as a myco issue + todo in
the `project/` ledger, a **fix** agent works the issues, and
**verify** re-runs the lint gate + battery — issues close on green,
or stay open and the loop retries (bounded).

```
start → attack ─┬─ attack.clean ─────────────────────┐
                └─ attack.findings → triage → fix → verify ─┬─ verify.passed → close ─┐
                                       ↑                    └─ verify.failed ─────────┤
                                       └──────────── (loop back to fix, max 3)         ↓
                                          abort on error / exhausted          report → adversarial.complete
```

## Running it

```bash
# secrets the campaign needs arrive via tamias run — the dogfood path.
# declared surface lives in the repo-root tamias.toml `[secrets]`:
tamias run --env dev -- peregrin campaign run campaigns/adversarial/main.yaml

# fake secrets are all it needs — a one-time dev-vault bootstrap:
printf 'fake-devin-token' | tamias put tamias://agents/devin
printf 'fake-cf-token'    | tamias put tamias://cf/token

# validate / dry-run without executing nodes:
peregrin campaign validate campaigns/adversarial/main.yaml
peregrin campaign run campaigns/adversarial/main.yaml --dry-run
```

Run from the **repo root** — node commands are relative paths.

`AGENT_RUNNER` (env, default `devin`) picks the agent-driver runner;
it's a normal env var, so `AGENT_RUNNER=claude tamias run --env dev --
peregrin campaign run …` works.

## Files

| File | Role |
| --- | --- |
| `main.yaml` | the campaign — nodes, events, emit wiring |
| `attack.sh` | the battery: T-008/T-009 repro patterns as checks A-01..A-12 (+`--full` = `cargo xtask test`) → `target/adversarial/findings.ndjson`; rc 0/1/2 = clean/findings/error |
| `triage.sh` | findings → `myco issue new` + `myco todo add`, dedup'd → `target/adversarial/issues.txt` |
| `verify.sh` | `cargo xtask check` + `attack.sh` re-run → rc 0/1/2 |
| `close.sh` | `myco issue resolve` for everything verify green-lit (best-effort) |
| `pdp-campaign.sh` | campaign-local policy decider for `tamias serve`'s `[serve.pdp] exec` wire — the stand-in until `peregrin policy decide` exists |
| `serve.pdp.toml` | example `tamias serve` config wiring `[serve.pdp] exec` to that decider |

## What the battery checks (repro patterns from prior audits)

| Check | Invariant | Audit provenance |
| --- | --- | --- |
| A-01 | deny-all PDP blocks get/**list**/**check**; manifest is a meta-op | T-009 (list/check consult) |
| A-02 | doomed request → `bad_request`, never spawns a consult | T-009 |
| A-03 | hung PDP killed at `timeout_ms` → `pdp_unreachable`, fail-closed | T-009 F-1 |
| A-04 | verdict >64 KiB → `pdp_unreachable` | T-009 F-2 |
| A-05 | unparseable verdict → fail-closed; `allow_on_error` degrades *openly* | T-009 |
| A-06 | PDP child env scrubbed — `TAMIAS_*`/deploy tokens never cross | T-009 F-3 |
| A-07 | request line >4 MiB → `bad_request` + reader resync | T-009 F-4 |
| A-08 | actor >256 B / ref >1 KiB → `bad_request`, no consult | T-009 F-5 |
| A-09 | `tamias run` child env = allowlist + declared `[secrets]` only | T-006 HIGH-2 |
| A-10 | secret values never land in the audit ndjson stream | invariant |
| A-11 | implicit `./tamias.toml` carrying `[serve.pdp]` warns (trust root) | T-009 F-6 |
| A-12 | permissive vault file (0666) → non-fatal stderr note | T-008 F-3 |
| A-FULL | `cargo xtask test` green (incl. worker bun smoke when bun present) | T-006/T-008 suites |

The cf-worker repros (replay via `used_sig`, JSON header shadowing,
unknown-id timing oracle, 3→4-field HMAC) are covered by the worker
smoke suite that `cargo xtask test` runs when `bun` is installed —
the battery doesn't duplicate them; a red suite lands as A-FULL.

## The `tamias serve` ↔ peregrin PDP wire

`[serve.pdp] exec` (ADR-0003) is the real integration seam: before
every vault-touching op, serve spawns the configured command, sends
`{actor, attestation?, op, env, ref?}` on stdin, and reads
`{"allow": …}` on stdout — fail-closed on any malfunction.

Wire it up for a gated serve:

```bash
tamias serve --config campaigns/adversarial/serve.pdp.toml
```

`pdp-campaign.sh` is the stand-in decider (non-prod rw allowed,
prod + rm/rotate denied). **The end-state is `exec = "peregrin
policy decide"`** — peregrin as the PDP is the design intent
(ADR-0003 names it), but peregrin has no policy verb yet; the gap is
recorded in the project ledger.

## Peregrin-version caveats (honest status)

- **main** (installed `peregrin` binary): `peregrin-engine`,
  `peregrin-loader-yaml`, `peregrin-driver-*` are stubs — campaigns
  cannot be loaded or run there.
- **v2 branch**: full port — `peregrin campaign run|validate`,
  drivers `shell`/`agent`/`http`/`human`, include/snippet expansion.
  This campaign is written against v2.
- v2 gotchas the campaign is built around: the `shell` driver
  hardcodes a 30 s timeout (so slow steps are `agent` nodes), and
  `${config.*}`/`${state.*}`/`${event.*}` scope lookups pass through
  unresolved (so state travels via files under `target/adversarial/`,
  and `${env("VAR","default")}` — capability-gated — is the only
  templating used).
