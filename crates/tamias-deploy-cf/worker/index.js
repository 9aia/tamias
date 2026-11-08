// tamias-registry-cf worker — a token-guarded R2 blob store. The tamias
// client maintains index.json (read-modify-write); this just stores blobs.
//
// Bindings: REGISTRY (R2 bucket), REGISTRY_TOKEN (secret). Deploy with
// `wrangler deploy` (see wrangler.toml).
export default {
  async fetch(request, env) {
    const auth = request.headers.get("authorization") || "";
    const token = auth.replace(/^Bearer\s+/i, "");
    if (env.REGISTRY_TOKEN && token !== env.REGISTRY_TOKEN) {
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
      const body = await request.arrayBuffer();
      await env.REGISTRY.put(key, body);
      return new Response("ok", { status: 200 });
    }
    return new Response("method not allowed", { status: 405 });
  },
};
