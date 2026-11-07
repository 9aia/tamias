# tamias-vault-cf worker

The server half of the `cf` vault kind: a Cloudflare Worker fronting
D1 that stores **opaque age ciphertext only**. All encryption and
decryption happens client-side in `tamias-vault-cf`; this worker has no
key material and cannot read the values it serves — plaintext never
crosses its boundary.

## Wire protocol (`tamias-vault-cf/2`)

Two auth schemes; both are accepted during migration (P-003):

- `Authorization: Bearer <token>` — legacy mode.
- `Authorization: TamiasHmac <token_id>, <ts>, <nonce>, <sig>` — the
  token **signs** instead of traveling:
  `sig = hex HMAC-SHA256(token, method + "\n" + path + "\n" + ts + "\n" + nonce + "\n" + sha256hex(body))`,
  `ts` is unix seconds, `|now − ts| ≤ 60s` (replay window), and a
  verified sig may be used **once** — the worker records
  `sha256(sig)` in the `used_sig` D1 table (~120s TTL, cleaned
  periodically) and rejects repeats with 401. The `nonce` keeps a
  legit retry alive: two identical requests inside the same second
  sign differently, so a retry isn't a replay. The pre-nonce
  three-field form `<token_id>, <ts>, <sig>` (signed without the
  nonce line) stays accepted while old clients migrate.

All comparisons are constant-time (P-006): byte-decode, XOR-fold
over the longer input, length folded in — no early exit.

Tokens are **scoped per env** (P-003 phase 1): the worker reads a
JSON token table from the `TAMIAS_VAULT_TOKENS` secret mapping
`token_id → {token, scopes}`, where scopes is `{env|"*" →
{read, write}}`. A leaked `dev` token can't read `prod`.

| Verb | Path | Body → Response |
| --- | --- | --- |
| `GET` | `/v1/{env}` | `{"refs": [{"key","name"}, …]}` |
| `GET` | `/v1/{env}/{key}` | `{"ciphertext","name","version","created_at","rotated_at"}` |
| `PUT` | `/v1/{env}/{key}` | `{"ciphertext","name"?,"rotate"}` → record metadata |
| `DELETE` | `/v1/{env}/{key}` | `{"removed": bool}` |

`{key}` is the storage key — a ref path (`billing/stripe`), or a
64-hex `ref_key` when the client runs `private_refs` (P-004); the
`name` column then carries an age ciphertext of the real ref so
`list` can map keys back. The worker treats both verbatim.

`PUT` with `"rotate": false` upserts (new → `version=1`; existing →
`version+1`, `created_at` preserved, `rotated_at` untouched).
`"rotate": true` additionally stamps `rotated_at` and 404s on a
missing key. Bookkeeping is server-side so writers can't race
client-computed versions. Errors are `{"error": "…"}` + non-2xx.

The reference implementation of these semantics for tests lives in
`tamias-testkit`'s `mocks::vault_wire_v2` — keep the two in sync.

## Smoke test

`index.test.js` executes **this file** — the real worker, not the
Rust mock — over a `bun:sqlite` shim for the D1 binding, applying
the real `schema.sql`. It covers auth (bearer + both TamiasHmac
forms), the replay guard, scope semantics, versioning, and the
route/error surface. It exists because the mock reimplements the
semantics and can't catch a worker-only bug (e.g. F1: a `const
JSON` shadow once made every request 500 while the Rust suite
stayed green).

```bash
cd crates/tamias-vault-cf/worker
bun test            # or, from the repo root: cargo xtask worker
```

`cargo xtask test` runs it automatically when `bun` is installed.

## Deploy

```bash
cd crates/tamias-vault-cf/worker

# 1. create + migrate the database
wrangler d1 create tamias-vault          # prints a database_id
#    → paste it into wrangler.toml [[d1_databases]]
wrangler d1 execute tamias-vault --file=schema.sql
#    (upgrading a v1 db: ALTER TABLE secrets ADD COLUMN name TEXT)
#    (pre-replay-guard db: CREATE TABLE used_sig — see schema.sql)

# 2. set the tokens (Workers secrets, never in the repo)
wrangler secret put TAMIAS_VAULT_TOKEN   # legacy single bearer, or:
wrangler secret put TAMIAS_VAULT_TOKENS  # scoped JSON map — see below

# 3. ship it
wrangler deploy
# → https://tamias-vault.<you>.workers.dev
```

`TAMIAS_VAULT_TOKENS` shape — `token_id` keys are what clients send in
`TamiasHmac <token_id>, …` (by convention, the `tamias://` ref path of
the token in the local vault):

```json
{
  "ops/cf-token-dev": {
    "token": "<dev-token-secret>",
    "scopes": { "dev": { "read": true, "write": true } }
  },
  "ops/cf-token-viewer": {
    "token": "<viewer-secret>",
    "scopes": { "*": { "read": true, "write": false } }
  }
}
```

**Rotation = `wrangler secret put`** — paste the new token into
`TAMIAS_VAULT_TOKENS` (and `tamias put` the new value at the token ref
locally). There is deliberately no admin route.

## Point tamias at it

```toml
# tamias.toml
[vaults.cloud]
kind = "cf"
url = "https://tamias-vault.<you>.workers.dev"
token_env = "TAMIAS_CF_TOKEN"        # env var name, not the token
                                   # (legacy bearer fallback)
identity = "~/.config/tamias/identity.txt"   # optional; same default as local
recipients = ["age1…"]             # optional extra encrypt-to keys
private_refs = true                # optional; hide ref names (P-004)

# optional per-env scoped tokens (P-003): token *refs* in the local
# vault — the stored value is the HMAC secret, the ref path is the
# token_id the worker's TAMIAS_VAULT_TOKENS map keys on.
[vaults.cloud.tokens]
dev = "tamias://ops/cf-token-dev"
prod = "tamias://ops/cf-token-prod"

[envs]
prod = { vault = "cloud" }
```

```bash
export TAMIAS_CF_TOKEN=<the same token>   # bearer fallback
tamias put tamias://billing/stripe --env prod   # encrypts, PUTs ciphertext
tamias list --env prod                        # reads ciphertext rows back

# upgrading an existing vault to private refs:
tamias migrate --private-refs               # re-keys plaintext rows
```

`tamias check` probes the vault with a `list` call — an unreachable or
mis-tokened worker, or a **mixed-mode vault** (private config +
plaintext rows) reports `ok: false`, never a panic.
