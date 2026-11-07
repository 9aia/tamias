// tamias-vault-cf worker — the `tamias-vault-cf/2` wire protocol over
// Cloudflare Workers + D1. See ../src/lib.rs for the client.
//
// THE WORKER NEVER SEES PLAINTEXT. Values arrive as base64 age
// ciphertext produced client-side by tamias-vault-cf and are stored
// verbatim in D1 rows; this file contains no decryption capability
// and no key material. Bookkeeping (version bump, created_at
// preservation, rotated_at stamping) is server-side so concurrent
// writers can't race client-computed versions. When the client runs
// with `private_refs = true` (P-004) even the `{key}` path segment
// is an opaque HMAC-derived hex string and the row's `name` column
// is an age-encrypted blob — the store can't tell which secrets
// exist, only that some do.
//
// Routes (all under /v1, authed + scoped):
//   GET    /v1/{env}           → {"refs": [{"key","name"}, …]}
//   GET    /v1/{env}/{key…}    → {"ciphertext","name","version","created_at","rotated_at"}
//   PUT    /v1/{env}/{key…}    {"ciphertext","name"?,"rotate"} → record metadata
//   DELETE /v1/{env}/{key…}    → {"removed": bool}
// Errors: {"error": "…"} with a non-2xx status.
//
// Auth — wire `tamias-vault-cf/2` (P-003):
//   Authorization: Bearer <token>
//     Legacy mode, still accepted during migration.
//   Authorization: TamiasHmac <token_id>, <ts>, <nonce>, <sig>
//     sig = hex HMAC-SHA256(token, method+"\n"+path+"\n"+ts+"\n"+nonce+"\n"+sha256hex(body)).
//     The token never travels; |now - ts| must be ≤ 60s (replay window)
//     AND a verified sig may be used once — replays die on the
//     `used_sig` D1 table (MED-1). The nonce makes two identical
//     requests in the same second sign differently — a legit retry
//     isn't a replay (F7). The pre-nonce 3-field form
//     `<token_id>, <ts>, <sig>` (signed without the nonce line) is
//     still accepted while old clients migrate.
//
// Token config — `wrangler secret put TAMIAS_VAULT_TOKENS` (JSON):
//   {"<token_id>": {"token": "<secret>",
//                   "scopes": {"<env>"|"*": {"read": true, "write": true}}}}
// `TAMIAS_VAULT_TOKEN` (single string secret) is the legacy fallback:
// one bearer token with read+write on every env. Rotation is a
// `wrangler secret put` away — no admin route.

// Response header block — NEVER name this `JSON`: shadowing the
// global makes every JSON.parse/stringify throw and every request
// 500 (F1). The worker smoke test (index.test.js) exists because
// the Rust wire mock can't catch a bug that lives only here.
const JSON_HEADERS = { "content-type": "application/json" };

const REPLAY_WINDOW_S = 60;
// Rows in `used_sig` outlive the ts window so a replayed request
// can't slip back in after the window rolls (MED-1).
const SIG_TTL_S = 120;
// Bound the pre-auth body read — an unauthenticated client must
// not be able to force a giant request.text() allocation (L1).
const MAX_BODY_BYTES = 4 * 1024 * 1024;
// How often the used_sig table is swept — amortized, not per-request.
const CLEANUP_EVERY_N = 50;
let reqCount = 0;

function json(body, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: JSON_HEADERS });
}

function err(status, msg) {
  return json({ error: msg }, status);
}

function hex(bytes) {
  return [...new Uint8Array(bytes)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

// Constant-time string equality (P-006): both sides are byte-decoded
// and XOR-folded over the *longer* length — no early exit on the
// first mismatching byte, and a length difference is folded into the
// accumulator instead of short-circuiting. Missing bytes read as 0.
function ctEq(a, b) {
  const enc = new TextEncoder();
  const x = enc.encode(a);
  const y = enc.encode(b);
  let diff = x.length ^ y.length;
  const n = Math.max(x.length, y.length);
  for (let i = 0; i < n; i++) {
    diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  }
  return diff === 0;
}

async function sha256Hex(text) {
  return hex(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text)));
}

async function hmacSha256Hex(secret, message) {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  return hex(await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(message)));
}

// The scoped-token table from `TAMIAS_VAULT_TOKENS` JSON. A malformed
// secret yields an empty table — requests then fail closed (401),
// never silently fall through to legacy auth. The table is
// prototype-less (L4): a "__proto__" key in the JSON can't pollute
// lookups via Object.assign's setter path.
function tokenTable(env) {
  const table = Object.create(null);
  if (typeof env.TAMIAS_VAULT_TOKENS === "string" && env.TAMIAS_VAULT_TOKENS) {
    try {
      const parsed = JSON.parse(env.TAMIAS_VAULT_TOKENS);
      if (parsed && typeof parsed === "object") {
        for (const id of Object.keys(parsed)) {
          if (Object.hasOwn(parsed, id)) table[id] = parsed[id];
        }
      }
    } catch {
      // invalid JSON → empty table → every token lookup misses
    }
  }
  return table;
}

const FULL_SCOPE = { read: true, write: true };

// Verify the Authorization header against the token table plus the
// legacy `TAMIAS_VAULT_TOKEN`. Returns the caller's scope map or null.
async function authenticate(request, env, path, bodyText) {
  const header = request.headers.get("authorization") || "";
  const table = tokenTable(env);

  if (header.startsWith("Bearer ")) {
    const candidate = header.slice("Bearer ".length);
    let scopes = null;
    let matched = false;
    // Compare against every configured token — no early exit on a
    // non-match, so the loop count doesn't reveal which entry was
    // hit. A shared value resolves to the last matching entry.
    for (const id of Object.keys(table)) {
      const entry = table[id];
      if (entry && typeof entry.token === "string" && ctEq(candidate, entry.token)) {
        matched = true;
        scopes = entry.scopes || null;
      }
    }
    // Legacy fallback applies only when NO scoped entry matched —
    // otherwise a shared token value would silently upgrade a
    // narrow scope to full read+write (L3). Narrower wins.
    if (!matched && typeof env.TAMIAS_VAULT_TOKEN === "string" && env.TAMIAS_VAULT_TOKEN) {
      if (ctEq(candidate, env.TAMIAS_VAULT_TOKEN)) {
        scopes = { "*": FULL_SCOPE };
      }
    }
    return scopes;
  }

  if (header.startsWith("TamiasHmac ")) {
    const parts = header
      .slice("TamiasHmac ".length)
      .split(",")
      .map((s) => s.trim());
    // Current form: `<id>, <ts>, <nonce>, <sig>` — the nonce is
    // signed material so identical requests in one second differ
    // (F7). The pre-nonce `<id>, <ts>, <sig>` form stays accepted
    // during migration, signed without the nonce line.
    if (parts.length !== 3 && parts.length !== 4) return null;
    const [id, ts] = parts;
    const nonce = parts.length === 4 ? parts[2] : "";
    const sig = parts.length === 4 ? parts[3] : parts[2];
    if (parts.length === 4 && !nonce) return null; // empty nonce ≠ 3-field form
    const t = Number(ts);
    const now = Math.floor(Date.now() / 1000);
    if (!Number.isFinite(t) || Math.abs(now - t) > REPLAY_WINDOW_S) return null;
    // Uniform work (F8): an unknown token id must not short-circuit
    // before the HMAC — the id→known/unknown timing oracle. Sign
    // with a throwaway key derived from the id itself and let the
    // compare fail on its own.
    const entry = Object.hasOwn(table, id) ? table[id] : null;
    const key =
      entry && typeof entry.token === "string" ? entry.token : `tamias-unknown-id:${id}`;
    const signed =
      parts.length === 4
        ? `${request.method}\n${path}\n${ts}\n${nonce}\n${await sha256Hex(bodyText)}`
        : `${request.method}\n${path}\n${ts}\n${await sha256Hex(bodyText)}`;
    const expect = await hmacSha256Hex(key, signed);
    if (!entry || !ctEq(sig, expect)) return null;
    // Replay guard (MED-1): a signature may only be used once.
    // INSERT OR IGNORE loses on the second sighting — the PK is
    // the sig's own hash, so two concurrent deliveries can't both
    // pass. A legit retry signs a fresh ts → a fresh sig.
    const sigHash = await sha256Hex(sig);
    const { meta } = await env.DB.prepare(
      "INSERT OR IGNORE INTO used_sig (sig, seen_at) VALUES (?, ?)"
    )
      .bind(sigHash, now)
      .run();
    if ((meta?.changes ?? 0) === 0) return null; // replay
    if (++reqCount % CLEANUP_EVERY_N === 0) {
      await env.DB.prepare("DELETE FROM used_sig WHERE seen_at < ?")
        .bind(now - SIG_TTL_S)
        .run();
    }
    return entry.scopes || null;
  }

  return null;
}

// `scopes` is `{env|"*": {read, write}}` — env entries win over the
// wildcard; a missing capability denies.
function scopeAllows(scopes, envName, write) {
  if (!scopes || typeof scopes !== "object") return false;
  // Own-property lookups only (F5): a plain `scopes[envName]` walk
  // hits the prototype chain, so a request for env "constructor"
  // or "__proto__" would "find" Object.prototype members. And an
  // explicit env entry wins even when falsy (F4): `"prod": null`
  // is a configured *deny*, not an invitation to inherit "*".
  const s = Object.hasOwn(scopes, envName)
    ? scopes[envName]
    : Object.hasOwn(scopes, "*")
      ? scopes["*"]
      : null;
  if (!s || typeof s !== "object") return false;
  return write ? s.write === true : s.read === true;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const rest = url.pathname.startsWith("/v1/")
      ? url.pathname.slice("/v1/".length)
      : null;
    if (rest === null) return err(404, "no such route");

    // /v1/{env} lists; /v1/{env}/{key…} addresses one record. Key
    // paths contain '/' by design (a ref path, or one opaque
    // HMAC-derived hex segment under private_refs).
    const slash = rest.indexOf("/");
    const envName = slash === -1 ? rest : rest.slice(0, slash);
    const keyPath = slash === -1 ? null : rest.slice(slash + 1);
    if (!envName || keyPath === "") return err(404, "no such route");

    const isRead = request.method === "GET";
    const isWrite = request.method === "PUT" || request.method === "DELETE";
    if (!isRead && !isWrite) return err(405, "method not allowed");

    // Auth after routing so the scope check knows env+verb; the
    // body text is read once — it feeds both signature
    // verification and the PUT handler. Bounded read (L1): a fast
    // content-length reject, then a hard cap on the stream itself
    // so a chunked body can't lie.
    const declared = Number(request.headers.get("content-length") || "0");
    if (declared > MAX_BODY_BYTES) return err(413, "payload too large");
    let bodyText;
    let scopes;
    try {
      if (request.body && request.body.getReader) {
        const reader = request.body.getReader();
        const chunks = [];
        let total = 0;
        let over = false;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          total += value.byteLength;
          if (total > MAX_BODY_BYTES) {
            over = true;
            break;
          }
          chunks.push(value);
        }
        if (over) {
          await reader.cancel();
          return err(413, "payload too large");
        }
        const buf = new Uint8Array(total);
        let off = 0;
        for (const c of chunks) {
          buf.set(c, off);
          off += c.byteLength;
        }
        bodyText = new TextDecoder().decode(buf);
      } else {
        bodyText = await request.text();
      }
      // authenticate() runs D1 (the replay guard) — a throw here
      // escaped the route try/catch below and became a 500 carrying
      // backend internals (F9). Same static message as the route arm.
      scopes = await authenticate(request, env, url.pathname, bodyText);
    } catch {
      return err(500, "backend error");
    }
    if (scopes === null) return err(401, "unauthorized");
    if (!scopeAllows(scopes, envName, isWrite)) {
      return err(403, "token scope denies this env/verb");
    }

    const db = env.DB;

    try {
      if (keyPath === null) {
        const { results } = await db
          .prepare("SELECT ref, name FROM secrets WHERE env = ? ORDER BY ref")
          .bind(envName)
          .all();
        return json({ refs: results.map((r) => ({ key: r.ref, name: r.name })) });
      }

      switch (request.method) {
        case "GET": {
          const row = await db
            .prepare(
              "SELECT ciphertext, name, version, created_at, rotated_at FROM secrets WHERE env = ? AND ref = ?"
            )
            .bind(envName, keyPath)
            .first();
          if (!row) return err(404, "not found");
          return json(row);
        }

        case "PUT": {
          let body;
          try {
            body = JSON.parse(bodyText);
          } catch {
            return err(400, "bad json body");
          }
          if (typeof body.ciphertext !== "string" || !body.ciphertext) {
            return err(400, "missing ciphertext");
          }
          // `name` is the private-refs name blob (age ciphertext,
          // opaque here); null on plain rows.
          const name = typeof body.name === "string" ? body.name : null;
          const rotate = body.rotate === true;
          const now = new Date().toISOString();

          const old = await db
            .prepare(
              "SELECT version, created_at, rotated_at, name FROM secrets WHERE env = ? AND ref = ?"
            )
            .bind(envName, keyPath)
            .first();

          if (!old && rotate) return err(404, "not found");
          // L8: a private row must keep its name blob — a plain
          // PUT with name:null would silently un-private the row
          // (and vice versa is mixed-mode). Reject either flip.
          if (old && old.name != null && name === null) {
            return err(409, "row carries a private name blob — PUT must include it");
          }
          if (old && old.name == null && name !== null) {
            return err(409, "row has no name blob — PUT must not add one");
          }

          // The version bump is atomic (F6): a SELECT-then-INSERT
          // pair lets two concurrent PUTs each compute
          // old.version+1 and lose a generation. The upsert derives
          // the next version from the row *at write time*;
          // rotated_at is stamped only on rotate writes (NULL is
          // "keep the stored stamp"), and created_at is untouched
          // on conflict — the winning INSERT's value survives.
          // RETURNING reports what actually landed, not what the
          // pre-read `old` believed.
          const row = await db
            .prepare(
              `INSERT INTO secrets (env, ref, ciphertext, name, version, created_at, rotated_at)
               VALUES (?, ?, ?, ?, 1, ?, ?)
               ON CONFLICT (env, ref) DO UPDATE SET
                 ciphertext = excluded.ciphertext,
                 name = excluded.name,
                 version = secrets.version + 1,
                 rotated_at = COALESCE(excluded.rotated_at, secrets.rotated_at)
               RETURNING version, created_at, rotated_at`
            )
            .bind(envName, keyPath, body.ciphertext, name, now, rotate ? now : null)
            .first();

          return json(row);
        }

        case "DELETE": {
          const { meta } = await db
            .prepare("DELETE FROM secrets WHERE env = ? AND ref = ?")
            .bind(envName, keyPath)
            .run();
          return json({ removed: (meta?.changes ?? 0) > 0 });
        }

        default:
          return err(405, "method not allowed");
      }
    } catch {
      // A D1/infra failure — a static message only; the raw
      // exception could carry schema/query internals (L2).
      return err(500, "backend error");
    }
  },
};
