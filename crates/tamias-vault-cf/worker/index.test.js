// Smoke test for the REAL worker/index.js — the Rust wire mock
// (tamias-testkit::mocks::vault_wire_v2) reimplements the semantics
// and can't catch a bug that lives only in this file. Case in
// point (F1): `const JSON` shadowed the global → every request
// 500'd while the whole Rust suite stayed green.
//
// The D1 binding is shimmed over bun:sqlite — the same SQLite
// dialect D1 runs, executing the real schema.sql. Everything else
// (Request/Response, crypto.subtle, TextEncoder) is standard web
// API the worker uses verbatim.
//
// Run:  bun test            (in this directory)
//   or: cargo xtask worker  (repo root)
// All tokens and values are FAKE, by rule.

import { test, expect } from "bun:test";
import { Database } from "bun:sqlite";
import { readFileSync } from "node:fs";
import worker from "./index.js";

const enc = new TextEncoder();

function hex(bytes) {
  return [...new Uint8Array(bytes)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

// D1's prepare → .bind(...) → {run, first, all} over bun:sqlite.
// The DDL is the real schema.sql — the file a deploy applies.
function makeDb() {
  const db = new Database(":memory:");
  db.exec(readFileSync(new URL("./schema.sql", import.meta.url), "utf8"));
  return {
    prepare(sql) {
      const stmt = db.prepare(sql);
      const call = (params) => ({
        run: () => ({ meta: { changes: stmt.run(...params).changes } }),
        first: () => stmt.get(...params) ?? null,
        all: () => ({ results: stmt.all(...params) }),
      });
      return {
        bind: (...params) => call(params),
        run: () => call([]).run(),
        first: () => call([]).first(),
        all: () => call([]).all(),
      };
    },
  };
}

// A worker `env`: real D1 shim + FAKE token config.
function makeEnv(tokens) {
  return {
    DB: makeDb(),
    TAMIAS_VAULT_TOKEN: "fake-legacy-token",
    TAMIAS_VAULT_TOKENS: tokens ? JSON.stringify(tokens) : undefined,
  };
}

const BEARER = { authorization: "Bearer fake-legacy-token" };

// Client-side TamiasHmac — `nonce` present → the current 4-field
// form; omitted → the pre-nonce 3-field legacy form.
async function hmacAuth(id, secret, method, path, body = "", opts = {}) {
  const ts = opts.ts ?? Math.floor(Date.now() / 1000);
  const key = await crypto.subtle.importKey(
    "raw",
    enc.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );
  const bodySha = hex(await crypto.subtle.digest("SHA-256", enc.encode(body)));
  const msg =
    opts.nonce === undefined
      ? `${method}\n${path}\n${ts}\n${bodySha}`
      : `${method}\n${path}\n${ts}\n${opts.nonce}\n${bodySha}`;
  const sig = hex(await crypto.subtle.sign("HMAC", key, enc.encode(msg)));
  return opts.nonce === undefined
    ? `TamiasHmac ${id}, ${ts}, ${sig}`
    : `TamiasHmac ${id}, ${ts}, ${opts.nonce}, ${sig}`;
}

const fetch = (env, url, init = {}) => worker.fetch(new Request(`http://w${url}`, init), env);

test("every request returns JSON, not a 500 (F1 smoke)", async () => {
  const env = makeEnv();
  const res = await fetch(env, "/v1/dev");
  expect(res.status).toBe(401);
  expect(res.headers.get("content-type")).toContain("application/json");
  const body = await res.json();
  expect(typeof body.error).toBe("string");
});

test("bearer CRUD roundtrip; versions increment, rotate stamps (F1, F6)", async () => {
  const env = makeEnv();
  const put = (body) =>
    fetch(env, "/v1/dev/a/b", {
      method: "PUT",
      headers: BEARER,
      body: JSON.stringify(body),
    });

  let res = await put({ ciphertext: "Zm9v" });
  expect((await res.json()).version).toBe(1);
  res = await put({ ciphertext: "YmFy" });
  expect((await res.json()).version).toBe(2);
  res = await put({ ciphertext: "YmF6", rotate: true });
  let meta = await res.json();
  expect(meta.version).toBe(3);
  expect(meta.rotated_at).toBeTruthy();
  // a plain PUT bumps the version but keeps the stored rotated_at
  res = await put({ ciphertext: "YmFj" });
  meta = await res.json();
  expect(meta.version).toBe(4);
  expect(meta.rotated_at).toBeTruthy();
  // created_at survived from the first write
  expect(meta.created_at).toBeTruthy();

  res = await fetch(env, "/v1/dev/a/b", { headers: BEARER });
  expect((await res.json()).ciphertext).toBe("YmFj");
  res = await fetch(env, "/v1/dev", { headers: BEARER });
  expect((await res.json()).refs.length).toBe(1);
  res = await fetch(env, "/v1/dev/a/b", { method: "DELETE", headers: BEARER });
  expect((await res.json()).removed).toBe(true);
  res = await fetch(env, "/v1/dev/a/b", { headers: BEARER });
  expect(res.status).toBe(404);
});

test("nonce-signed requests pass; an identical replay is spent (F7)", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { dev: { read: true, write: true } },
    },
  });
  const ts = Math.floor(Date.now() / 1000);
  const auth1 = await hmacAuth("ops/tok", "fake-hmac-secret", "GET", "/v1/dev", "", {
    nonce: "n-1",
    ts,
  });
  const auth2 = await hmacAuth("ops/tok", "fake-hmac-secret", "GET", "/v1/dev", "", {
    nonce: "n-2",
    ts,
  });
  let res = await fetch(env, "/v1/dev", { headers: { authorization: auth1 } });
  expect(res.status).toBe(200);
  // the identical signed request → same sig → the replay guard bites
  res = await fetch(env, "/v1/dev", { headers: { authorization: auth1 } });
  expect(res.status).toBe(401);
  // same second, fresh nonce → a different sig → NOT a replay
  res = await fetch(env, "/v1/dev", { headers: { authorization: auth2 } });
  expect(res.status).toBe(200);
  // a 4-field header with an empty nonce is rejected outright
  res = await fetch(env, "/v1/dev", {
    headers: { authorization: `TamiasHmac ops/tok, ${ts}, , deadbeef` },
  });
  expect(res.status).toBe(401);
});

test("pre-nonce 3-field TamiasHmac stays accepted during migration", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { "*": { read: true, write: false } },
    },
  });
  const auth = await hmacAuth("ops/tok", "fake-hmac-secret", "GET", "/v1/dev");
  const res = await fetch(env, "/v1/dev", { headers: { authorization: auth } });
  expect(res.status).toBe(200);
});

test("a falsy env scope entry denies — the wildcard can't rescue it (F4)", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { prod: null, "*": { read: true, write: true } },
    },
  });
  const auth = { authorization: "Bearer fake-hmac-secret" };
  const res = await fetch(env, "/v1/prod", { headers: auth });
  expect(res.status).toBe(403);
  // the wildcard still covers envs with no explicit entry at all
  const res2 = await fetch(env, "/v1/dev", { headers: auth });
  expect(res2.status).toBe(200);
});

test("prototype-named envs resolve via own props, not the proto chain (F5)", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { "*": { read: true, write: false } },
    },
  });
  const auth = { authorization: "Bearer fake-hmac-secret" };
  for (const weird of ["constructor", "toString", "__proto__", "hasOwnProperty"]) {
    // the wildcard grant must apply — an inherited Object.prototype
    // member is not a scope entry and must not mask the wildcard
    const res = await fetch(env, `/v1/${weird}`, { headers: auth });
    expect(res.status).toBe(200);
    // and write stays denied — "read-only" can't flip via proto luck
    const w = await fetch(env, `/v1/${weird}/k`, {
      method: "PUT",
      headers: auth,
      body: JSON.stringify({ ciphertext: "eQ" }),
    });
    expect(w.status).toBe(403);
  }
});

test("an unknown token id fails auth on the same path (F8)", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { "*": { read: true, write: true } },
    },
  });
  // well-formed signature under an id the table doesn't carry
  const auth = await hmacAuth("ghost", "whatever", "GET", "/v1/dev", "", { nonce: "n" });
  const res = await fetch(env, "/v1/dev", { headers: { authorization: auth } });
  expect(res.status).toBe(401);
});

test("a backend throw during auth → static 500, no internals (F9)", async () => {
  const env = makeEnv({
    "ops/tok": {
      token: "fake-hmac-secret",
      scopes: { "*": { read: true, write: true } },
    },
  });
  const realPrepare = env.DB.prepare.bind(env.DB);
  env.DB.prepare = (sql) => {
    if (sql.includes("used_sig")) {
      throw new Error("d1 dump: CREATE TABLE secrets (env, ref, ciphertext, ...)");
    }
    return realPrepare(sql);
  };
  const auth = await hmacAuth("ops/tok", "fake-hmac-secret", "GET", "/v1/dev", "", {
    nonce: "n",
  });
  const res = await fetch(env, "/v1/dev", { headers: { authorization: auth } });
  expect(res.status).toBe(500);
  const body = await res.text();
  expect(body).toBe('{"error":"backend error"}');
  expect(body).not.toContain("ciphertext");
});

test("route shape errors stay honest (404/405)", async () => {
  const env = makeEnv();
  let res = await fetch(env, "/nope", { headers: BEARER });
  expect(res.status).toBe(404);
  res = await fetch(env, "/v1/dev/x", { method: "PATCH", headers: BEARER });
  expect(res.status).toBe(405);
});
