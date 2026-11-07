-- D1 schema for the tamias-vault-cf worker.
-- ciphertext is base64 age ciphertext — THE STORE NEVER SEES
-- PLAINTEXT. Apply with:
--   wrangler d1 execute tamias-vault --file=schema.sql
CREATE TABLE IF NOT EXISTS secrets (
  env        TEXT NOT NULL,
  ref        TEXT NOT NULL,                -- ref path, or HMAC-derived
                                           -- ref_key when private_refs
  ciphertext TEXT NOT NULL,                -- base64 age ciphertext, opaque
  name       TEXT,                         -- private-refs name blob
                                           -- (base64 age ciphertext of
                                           -- the real ref); NULL on
                                           -- plain rows
  version    INTEGER NOT NULL DEFAULT 1,   -- server-side write generation
  created_at TEXT NOT NULL,                -- ISO 8601 / RFC 3339
  rotated_at TEXT,                         -- set by rotate writes only
  PRIMARY KEY (env, ref)
);

-- Replay guard for the TamiasHmac scheme (MED-1): a verified
-- signature's sha256 lands here once; a second sighting is a
-- replay and fails auth. Rows age out after ~120s (> the ±60s
-- ts window); cleanup is amortized in index.js.
CREATE TABLE IF NOT EXISTS used_sig (
  sig      TEXT PRIMARY KEY,               -- sha256 hex of the verified sig
  seen_at  INTEGER NOT NULL                -- unix seconds
);

-- Upgrading a v1 database? Add the name column once:
--   wrangler d1 execute tamias-vault --command="ALTER TABLE secrets ADD COLUMN name TEXT"
-- Upgrading a pre-replay-guard database? Add used_sig once:
--   wrangler d1 execute tamias-vault --command="CREATE TABLE IF NOT EXISTS used_sig (sig TEXT PRIMARY KEY, seen_at INTEGER NOT NULL)"
