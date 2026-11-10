// tamias-registry-cf worker — a token-guarded R2 blob store. The tamias
// client maintains index.json (read-modify-write); this just stores blobs.
//
// Bindings: REGISTRY (R2 bucket), REGISTRY_TOKEN (secret),
// MAX_UPLOAD_BYTES (var — the upload cap baked by `tamias
// deploy-registry`; falls back to the built-in default when absent).
// Deploy with `wrangler deploy` (see wrangler.toml).
//
// Auth FAILS CLOSED: if REGISTRY_TOKEN was never set, every request is
// rejected — an unconfigured secret must never mean "world-writable".

// Mirrors tamias-config's DEFAULT_MAX_UPLOAD (512 MiB).
const DEFAULT_MAX_UPLOAD = 512 * 1024 * 1024;

export default {
  async fetch(request, env) {
    if (!env.REGISTRY_TOKEN) {
      return new Response("registry token not configured", { status: 500 });
    }
    const maxUpload = Number(env.MAX_UPLOAD_BYTES) || DEFAULT_MAX_UPLOAD;
    const auth = request.headers.get("authorization") || "";
    const token = auth.replace(/^Bearer\s+/i, "");
    if (!(await tokenEquals(token, env.REGISTRY_TOKEN))) {
      return new Response("unauthorized", { status: 401 });
    }
    const url = new URL(request.url);
    const key = decodeURIComponent(url.pathname.replace(/^\/+/, "")) || "index.json";
    if (key.includes("..")) {
      return new Response("bad key", { status: 400 });
    }
    if (request.method === "GET") {
      const obj = await env.REGISTRY.get(key);
      if (!obj) return new Response("not found", { status: 404 });
      return new Response(obj.body, { status: 200 });
    }
    if (request.method === "PUT") {
      // Reject on the declared length before buffering the body, then
      // re-check the actual size — a lying/absent Content-Length can't
      // smuggle an over-limit blob into R2.
      const declared = Number(request.headers.get("content-length") || 0);
      if (declared > maxUpload) {
        return new Response("payload too large", { status: 413 });
      }
      const body = await request.arrayBuffer();
      if (body.byteLength > maxUpload) {
        return new Response("payload too large", { status: 413 });
      }
      await env.REGISTRY.put(key, body);
      return new Response("ok", { status: 200 });
    }
    return new Response("method not allowed", { status: 405 });
  },
};

// Constant-time-ish compare: SHA-256 both sides, then XOR the digests —
// the loop always runs 32 rounds regardless of where the strings differ,
// so a prefix guess can't be timed. (Workers have no timingSafeEqual.)
async function tokenEquals(given, expected) {
  const enc = new TextEncoder();
  const [a, b] = await Promise.all([
    crypto.subtle.digest("SHA-256", enc.encode(given)),
    crypto.subtle.digest("SHA-256", enc.encode(expected)),
  ]);
  const x = new Uint8Array(a);
  const y = new Uint8Array(b);
  let diff = 0;
  for (let i = 0; i < x.length; i++) diff |= x[i] ^ y[i];
  return diff === 0;
}
