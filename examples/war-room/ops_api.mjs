// Mock ops API the commander investigates against: service status, runbooks,
// paging, and mitigation execution. Plain Node http, loopback only.
// Run: node ops_api.mjs [port]
import http from "node:http";

const port = Number(process.argv[2] ?? 9911);

const state = {
  pages: [],
  mitigations: [],
};

const STATUS = {
  "checkout-api": {
    service: "checkout-api",
    healthy: false,
    errorRate: 0.34,
    p99LatencyMs: 4180,
    lastDeploy: { id: "deploy-8841", age: "14m", change: "enable new payment-intent cache" },
    upstreams: [
      { service: "payments-gateway", healthy: true },
      { service: "redis-sessions", healthy: false, note: "connection pool exhausted, 91% rejects" },
    ],
  },
  "redis-sessions": {
    service: "redis-sessions",
    healthy: false,
    errorRate: 0.91,
    p99LatencyMs: 12,
    note: "maxclients reached since 14m ago; correlates with checkout-api deploy-8841",
    upstreams: [],
  },
};

const RUNBOOKS = {
  "checkout-api": {
    service: "checkout-api",
    knownFailureModes: [
      "payment-intent cache misconfiguration can open one redis connection per request (introduced risk, see deploy checklist)",
      "gateway timeouts surface as 502 bursts",
    ],
    safeMitigations: [
      "roll back the most recent deploy (fully reversible, ~3m)",
      "feature-flag off payment-intent cache: POST /flags/payment_intent_cache/disable",
    ],
    doNot: ["restart redis-sessions during peak traffic — drops all live carts"],
  },
  "redis-sessions": {
    service: "redis-sessions",
    knownFailureModes: ["maxclients exhaustion from a misbehaving client service"],
    safeMitigations: ["identify and fix the offending client; do NOT bounce the cluster under load"],
    doNot: ["increase maxclients blindly (masks the leak until OOM)"],
  },
};

const server = http.createServer((req, res) => {
  const respond = (code, body) => {
    res.writeHead(code, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };
  const url = new URL(req.url, `http://127.0.0.1:${port}`);
  const [, root, name] = url.pathname.split("/");
  console.log(`[ops] ${req.method} ${url.pathname}`);

  if (root === "status") {
    return respond(200, STATUS[name] ?? { service: name, healthy: true, errorRate: 0 });
  }
  if (root === "runbook") {
    return respond(200, RUNBOOKS[name] ?? { service: name, safeMitigations: [] });
  }
  if (root === "page" && req.method === "POST") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const page = JSON.parse(body || "{}");
      state.pages.push(page);
      console.log(`[ops] PAGE -> secondary on-call`, page);
      respond(200, { paged: true, count: state.pages.length });
    });
    return;
  }
  if (root === "mitigate" && req.method === "POST") {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      const m = JSON.parse(body || "{}");
      state.mitigations.push(m);
      console.log(`[ops] MITIGATION EXECUTED:`, m);
      respond(200, { executed: true, ticket: `CHG-${1000 + state.mitigations.length}` });
    });
    return;
  }
  if (root === "state") return respond(200, state);
  respond(404, { error: "unknown route" });
});

server.listen(port, "127.0.0.1", () => console.log(`[ops] mock ops API on 127.0.0.1:${port}`));
