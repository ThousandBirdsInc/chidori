/**
 * Chidori run store on Cloudflare Durable Objects.
 *
 * One Durable Object per Chidori run. The run's journal (ordered CallRecords)
 * and auxiliary blobs (manifest, pending operation, host promises, signal
 * inbox, branch stores) live in the object's SQLite-backed storage, so every
 * write Chidori acknowledges has been replicated across data centers by
 * Cloudflare's Storage Relay Service and is covered by 30-day point-in-time
 * recovery.
 *
 * Protocol (mirrors `HttpRunStore` in crates/chidori/src/runtime/store.rs):
 *
 *   GET    /runs                          → ["run-id", ...]
 *   GET    /runs/:id/records              → [CallRecord, ...]   (404 if none)
 *   POST   /runs/:id/records              → append one record (body: record)
 *   PUT    /runs/:id/records              → replace the journal (body: array)
 *   GET    /runs/:id/blobs                → ["key", ...]
 *   GET    /runs/:id/blobs/:key           → blob bytes          (404 if none)
 *   PUT    /runs/:id/blobs/:key           → store blob bytes
 *   DELETE /runs/:id/blobs/:key           → remove blob
 *   GET    /registry                      → [entry, ...]        (agent names)
 *   GET    /registry/:name                → entry               (404 if none)
 *   PUT    /registry/:name                → store entry (body: entry JSON)
 *
 * Auth: when the CHIDORI_RUN_STORE_TOKEN secret is set, requests must carry
 * `Authorization: Bearer <token>`.
 *
 * The run index and agent registry live in a singleton object (name "@index")
 * so `GET /runs` doesn't need to enumerate the DO namespace (which the DO API
 * does not offer).
 */

export interface Env {
  RUN: DurableObjectNamespace;
  CHIDORI_RUN_STORE_TOKEN?: string;
}

const JSON_HEADERS = { "content-type": "application/json" };

export class ChidoriRun {
  private storage: DurableObjectStorage;

  constructor(state: DurableObjectState) {
    this.storage = state.storage;
    this.storage.sql.exec(
      `CREATE TABLE IF NOT EXISTS records (
         seq INTEGER PRIMARY KEY,
         pos INTEGER NOT NULL,
         data TEXT NOT NULL
       );
       CREATE TABLE IF NOT EXISTS blobs (
         key TEXT PRIMARY KEY,
         data BLOB NOT NULL
       );
       CREATE TABLE IF NOT EXISTS registry (
         name TEXT PRIMARY KEY,
         data TEXT NOT NULL
       );
       CREATE TABLE IF NOT EXISTS runs (
         run_id TEXT PRIMARY KEY
       );`,
    );
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    // Paths arrive re-rooted by the worker: /records, /blobs/:key,
    // /index/runs, /index/registry/:name.
    const segments = url.pathname.split("/").filter(Boolean);

    if (segments[0] === "records") {
      return this.records(request);
    }
    if (segments[0] === "blobs") {
      const key = decodeURIComponent(segments.slice(1).join("/"));
      return this.blobs(request, key);
    }
    if (segments[0] === "index") {
      return this.index(request, segments.slice(1));
    }
    return new Response("not found", { status: 404 });
  }

  private async records(request: Request): Promise<Response> {
    const sql = this.storage.sql;
    if (request.method === "GET") {
      const rows = sql.exec("SELECT data FROM records ORDER BY pos, seq").toArray();
      if (rows.length === 0) {
        return new Response("no journal", { status: 404 });
      }
      const body = `[${rows.map((r) => r.data as string).join(",")}]`;
      return new Response(body, { headers: JSON_HEADERS });
    }
    if (request.method === "POST") {
      const record = (await request.json()) as { seq: number };
      const next =
        ((sql.exec("SELECT MAX(pos) AS m FROM records").one().m as number) ?? 0) + 1;
      sql.exec(
        `INSERT INTO records (seq, pos, data) VALUES (?, ?, ?)
         ON CONFLICT(seq) DO UPDATE SET data = excluded.data`,
        record.seq,
        next,
        JSON.stringify(record),
      );
      return new Response("ok");
    }
    if (request.method === "PUT") {
      const records = (await request.json()) as Array<{ seq: number }>;
      sql.exec("DELETE FROM records");
      records.forEach((record, pos) => {
        sql.exec(
          "INSERT INTO records (seq, pos, data) VALUES (?, ?, ?)",
          record.seq,
          pos,
          JSON.stringify(record),
        );
      });
      return new Response("ok");
    }
    return new Response("method not allowed", { status: 405 });
  }

  private async blobs(request: Request, key: string): Promise<Response> {
    const sql = this.storage.sql;
    if (request.method === "GET" && key === "") {
      const rows = sql.exec("SELECT key FROM blobs ORDER BY key").toArray();
      return new Response(JSON.stringify(rows.map((r) => r.key)), {
        headers: JSON_HEADERS,
      });
    }
    if (key === "") {
      return new Response("blob key required", { status: 400 });
    }
    if (request.method === "GET") {
      const rows = sql.exec("SELECT data FROM blobs WHERE key = ?", key).toArray();
      if (rows.length === 0) {
        return new Response("not found", { status: 404 });
      }
      return new Response(rows[0].data as ArrayBuffer);
    }
    if (request.method === "PUT") {
      const data = await request.arrayBuffer();
      sql.exec(
        `INSERT INTO blobs (key, data) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET data = excluded.data`,
        key,
        data,
      );
      return new Response("ok");
    }
    if (request.method === "DELETE") {
      sql.exec("DELETE FROM blobs WHERE key = ?", key);
      return new Response("ok");
    }
    return new Response("method not allowed", { status: 405 });
  }

  /** The singleton index object: run listing + detached-agent registry. */
  private async index(request: Request, segments: string[]): Promise<Response> {
    const sql = this.storage.sql;
    if (segments[0] === "runs") {
      if (request.method === "GET") {
        const rows = sql.exec("SELECT run_id FROM runs ORDER BY run_id").toArray();
        return new Response(JSON.stringify(rows.map((r) => r.run_id)), {
          headers: JSON_HEADERS,
        });
      }
      if (request.method === "POST") {
        const { runId } = (await request.json()) as { runId: string };
        sql.exec("INSERT OR IGNORE INTO runs (run_id) VALUES (?)", runId);
        return new Response("ok");
      }
    }
    if (segments[0] === "registry") {
      const name = decodeURIComponent(segments.slice(1).join("/"));
      if (request.method === "GET" && name === "") {
        const rows = sql.exec("SELECT data FROM registry ORDER BY name").toArray();
        const body = `[${rows.map((r) => r.data as string).join(",")}]`;
        return new Response(body, { headers: JSON_HEADERS });
      }
      if (request.method === "GET") {
        const rows = sql.exec("SELECT data FROM registry WHERE name = ?", name).toArray();
        if (rows.length === 0) {
          return new Response("not found", { status: 404 });
        }
        return new Response(rows[0].data as string, { headers: JSON_HEADERS });
      }
      if (request.method === "PUT") {
        const data = await request.text();
        sql.exec(
          `INSERT INTO registry (name, data) VALUES (?, ?)
           ON CONFLICT(name) DO UPDATE SET data = excluded.data`,
          name,
          data,
        );
        return new Response("ok");
      }
    }
    return new Response("not found", { status: 404 });
  }
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (env.CHIDORI_RUN_STORE_TOKEN) {
      const auth = request.headers.get("authorization") ?? "";
      if (auth !== `Bearer ${env.CHIDORI_RUN_STORE_TOKEN}`) {
        return new Response("unauthorized", { status: 401 });
      }
    }

    const url = new URL(request.url);
    const segments = url.pathname.split("/").filter(Boolean);
    const indexStub = env.RUN.get(env.RUN.idFromName("@index"));

    // GET /runs → the singleton index object.
    if (segments[0] === "runs" && segments.length === 1) {
      return indexStub.fetch(new Request(`${url.origin}/index/runs`, request));
    }

    // /registry[...] → the singleton index object.
    if (segments[0] === "registry") {
      const rest = segments.slice(1).map(encodeURIComponent).join("/");
      return indexStub.fetch(
        new Request(`${url.origin}/index/registry/${rest}`, request),
      );
    }

    // /runs/:id/... → the run's own object, re-rooted past the id. First
    // write registers the run in the index so GET /runs can enumerate.
    if (segments[0] === "runs" && segments.length >= 2) {
      const runId = decodeURIComponent(segments[1]);
      const rest = segments.slice(2).join("/");
      if (request.method !== "GET") {
        await indexStub.fetch(
          new Request(`${url.origin}/index/runs`, {
            method: "POST",
            headers: JSON_HEADERS,
            body: JSON.stringify({ runId }),
          }),
        );
      }
      const stub = env.RUN.get(env.RUN.idFromName(runId));
      return stub.fetch(new Request(`${url.origin}/${rest}${url.search}`, request));
    }

    return new Response("not found", { status: 404 });
  },
} satisfies ExportedHandler<Env>;
