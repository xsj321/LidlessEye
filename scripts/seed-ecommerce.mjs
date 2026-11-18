#!/usr/bin/env node
// Synthetic e-commerce cluster seed for LidlessEye.
//
// It only talks to the ingest HTTP endpoints. No containers, no agents, no
// eBPF, no files on disk: every report and map delta is built in memory and
// posted straight to the server. A verification pass then reads the data back
// through the query API so a successful run means "the data is queryable in
// every time slice", not just "the server returned 200 on upload".
//
// Two profiles:
//   demo   8h, 19 services, shallow stacks — quick visual demo (default).
//   stress 30d, 60+ services, 60 deep stacks per report, 5-minute buckets —
//             load/query testing with a dense request histogram at every window.
//
// Usage:
//   node scripts/seed-ecommerce.mjs                       # demo profile
//   node scripts/seed-ecommerce.mjs --profile stress      # 30-day stress seed
//   node scripts/seed-ecommerce.mjs --profile stress --days 1   # short trial
//   # split a long run into segments that each fit one command timeout:
//   node scripts/seed-ecommerce.mjs --profile stress --from-hours-ago 720 --hours 120
//   node scripts/seed-ecommerce.mjs --profile stress --from-hours-ago 600 --hours 120
//   ... down to --from-hours-ago 120 --hours 120 (six 5-day segments)
//   node scripts/seed-ecommerce.mjs --tenant default
//   # all segments share one anchor so there are no seams:
//   ANCHOR=$(node -e 'process.stdout.write(String(Date.now()))')
//   node scripts/seed-ecommerce.mjs --profile stress --now-ms $ANCHOR --from-hours-ago 720 --hours 120
//   node scripts/seed-ecommerce.mjs --hours 4 --window 60 --services 30
//   node scripts/seed-ecommerce.mjs --map-only            # top up map deltas
//   node scripts/seed-ecommerce.mjs --no-verify
//   node scripts/seed-ecommerce.mjs --dry-run             # print the plan
//
// Env: LIDLESSEYE_BASE (default http://127.0.0.1:4040).

import process from "node:process";

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

function arg(name, fallback) {
  const flag = `--${name}`;
  const i = process.argv.indexOf(flag);
  if (i < 0) return fallback;
  const value = process.argv[i + 1];
  if (value === undefined || value.startsWith("--")) throw new Error(`${flag} needs a value`);
  return value;
}

const PROFILE = arg("profile", "demo");
const isStress = PROFILE === "stress";
if (!["demo", "stress"].includes(PROFILE)) throw new Error(`unknown --profile ${PROFILE}`);

// Stress defaults are deliberately large; every one is overridable.
const DEFAULTS = isStress
  ? { days: 30, window: 300, services: 64, stacks: 60, depth: 24, batch: 8, reports: 1500 }
  : { days: 0, hours: 8, window: 120, services: 19, stacks: 4, depth: 4, batch: 12, reports: 240 };

const BASE = (process.env.LIDLESSEYE_BASE || "http://127.0.0.1:4040").replace(/\/+$/, "");
const TENANT = arg("tenant", "shop-demo");
const HOURS = Number(arg("hours", DEFAULTS.hours ?? DEFAULTS.days * 24));
// `--from-hours-ago` lets a long run be split into segments that each fit a
// foreground/background command timeout. E.g. --from-hours-ago 720 --hours 240
// writes the oldest 10 days of a 30-day range.
const FROM_HOURS_AGO = Number(arg("from-hours-ago", HOURS));
const WINDOW_SEC = Number(arg("window", DEFAULTS.window));
const SERVICE_COUNT = Number(arg("services", DEFAULTS.services));
const STACKS_PER_REPORT = Number(arg("stacks", DEFAULTS.stacks));
const DEPTH = Number(arg("depth", DEFAULTS.depth));
const BATCH_WINDOWS = Number(arg("batch-windows", DEFAULTS.batch));
const REPORTS_PER_POST = Number(arg("reports-per-post", DEFAULTS.reports));
const DRY_RUN = process.argv.includes("--dry-run");
const NO_VERIFY = process.argv.includes("--no-verify");
const MAP_ONLY = process.argv.includes("--map-only");

if (!Number.isFinite(HOURS) || HOURS <= 0) throw new Error("--hours must be positive");
if (!Number.isFinite(WINDOW_SEC) || WINDOW_SEC < 10) throw new Error("--window must be >= 10s");
if (SERVICE_COUNT < 1 || STACKS_PER_REPORT < 1 || DEPTH < 2) throw new Error("bad profile sizes");

const HOUR_NS = 3_600_000_000_000;
const WINDOW_NS = WINDOW_SEC * 1_000_000_000;
const WINDOWS = Math.max(1, Math.floor((HOURS * HOUR_NS) / WINDOW_NS));

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

const NS = "shop";
const nodeId = (s) =>
  s.kind === "service" ? s.name : s.kind === "external" ? "external" : `${NS}/${s.name}`;

// Mid-size demo set: explicit, hand-named services and links.
const DEMO_SERVICES = [
  { name: "edge-proxy",     kind: "service",  ns: "edge",  weight: 1.4, replicas: 2, containers: ["envoy"],              node: "shop-edge-1" },
  { name: "frontend",       kind: "workload", ns: NS,      weight: 1.2, replicas: 4, containers: ["app"],                node: "shop-app-1" },
  { name: "api-gateway",    kind: "workload", ns: NS,      weight: 1.6, replicas: 3, containers: ["app", "istio-proxy"], node: "shop-app-2" },
  { name: "catalog",        kind: "workload", ns: NS,      weight: 1.0, replicas: 3, containers: ["app", "istio-proxy"], node: "shop-app-2" },
  { name: "search",         kind: "workload", ns: NS,      weight: 0.9, replicas: 2, containers: ["app"],                node: "shop-app-3" },
  { name: "cart",           kind: "workload", ns: NS,      weight: 1.1, replicas: 3, containers: ["app", "istio-proxy"], node: "shop-app-3" },
  { name: "checkout",       kind: "workload", ns: NS,      weight: 0.8, replicas: 2, containers: ["app", "istio-proxy"], node: "shop-app-1" },
  { name: "order",          kind: "workload", ns: NS,      weight: 0.7, replicas: 2, containers: ["app"],                node: "shop-app-4" },
  { name: "payment",        kind: "workload", ns: NS,      weight: 0.5, replicas: 2, containers: ["app", "istio-proxy"], node: "shop-app-4" },
  { name: "inventory",      kind: "workload", ns: NS,      weight: 0.6, replicas: 2, containers: ["app"],                node: "shop-app-5" },
  { name: "user",           kind: "workload", ns: NS,      weight: 0.8, replicas: 3, containers: ["app", "istio-proxy"], node: "shop-app-5" },
  { name: "recommendation", kind: "workload", ns: NS,      weight: 0.7, replicas: 2, containers: ["app"],                node: "shop-app-6" },
  { name: "review",         kind: "workload", ns: NS,      weight: 0.4, replicas: 2, containers: ["app"],                node: "shop-app-6" },
  { name: "notification",   kind: "workload", ns: NS,      weight: 0.3, replicas: 1, containers: ["app"],                node: "shop-app-6" },
  { name: "shipping",       kind: "workload", ns: NS,      weight: 0.4, replicas: 2, containers: ["app"],                node: "shop-app-7" },
  { name: "postgres",       kind: "service",  ns: NS,      weight: 0.9, replicas: 1, containers: ["postgres"],           node: "shop-data-1" },
  { name: "redis",          kind: "service",  ns: NS,      weight: 1.0, replicas: 1, containers: ["redis"],              node: "shop-data-2" },
  { name: "kafka",          kind: "service",  ns: NS,      weight: 0.6, replicas: 3, containers: ["kafka"],              node: "shop-data-3" },
  { name: "elasticsearch",  kind: "service",  ns: NS,      weight: 0.5, replicas: 2, containers: ["elasticsearch"],      node: "shop-data-4" },
];

const DEMO_LINKS = [
  ["external", "edge-proxy", "http", 1.3], ["edge-proxy", "frontend", "http", 1.3],
  ["frontend", "api-gateway", "http", 1.4], ["api-gateway", "catalog", "http", 1.0],
  ["api-gateway", "search", "http", 0.8], ["api-gateway", "cart", "http", 1.0],
  ["api-gateway", "checkout", "http", 0.7], ["api-gateway", "user", "http", 0.8],
  ["api-gateway", "recommendation", "grpc", 0.6], ["checkout", "order", "grpc", 0.6],
  ["checkout", "payment", "grpc", 0.5], ["checkout", "inventory", "grpc", 0.5],
  ["order", "postgres", "postgres", 0.6], ["order", "kafka", "kafka", 0.5],
  ["payment", "postgres", "postgres", 0.4], ["payment", "kafka", "kafka", 0.3],
  ["catalog", "postgres", "postgres", 0.7], ["catalog", "redis", "redis", 1.0],
  ["search", "elasticsearch", "http", 0.6], ["search", "redis", "redis", 0.4],
  ["cart", "redis", "redis", 0.9], ["user", "postgres", "postgres", 0.5],
  ["user", "redis", "redis", 0.6], ["recommendation", "redis", "redis", 0.5],
  ["recommendation", "kafka", "kafka", 0.3], ["review", "postgres", "postgres", 0.3],
  ["review", "elasticsearch", "http", 0.2], ["notification", "kafka", "kafka", 0.2],
  ["shipping", "order", "grpc", 0.2], ["shipping", "postgres", "postgres", 0.2],
].map(([client, server, protocol, weight]) => ({ client, server, protocol, weight }));

// Stress topology: many services across domains, generated deterministically.
const STRESS_DOMAINS = [
  ["edge", "service", ["edge-proxy", "waf", "lb", "cdn-origin"]],
  ["frontend", "workload", ["web", "mobile-api", "admin-ui", "partner-api"]],
  ["gateway", "workload", ["api-gateway", "bff", "graphql"]],
  ["identity", "workload", ["auth", "session", "token", "mfa", "oauth"]],
  ["catalog", "workload", ["catalog", "product", "price", "inventory", "search", "recommend", "category"]],
  ["commerce", "workload", ["cart", "checkout", "order", "payment", "refund", "coupon", "promotion", "tax"]],
  ["fulfillment", "workload", ["shipping", "warehouse", "tracking", "logistics", "delivery"]],
  ["user", "workload", ["profile", "address", "preference", "notification", "consent"]],
  ["social", "workload", ["review", "rating", "comment", "feed", "message"]],
  ["data", "service", ["postgres", "postgres-replica", "redis", "redis-cluster", "kafka", "elasticsearch", "clickhouse", "cassandra", "s3-gateway"]],
  ["infra", "service", ["dns", "metrics", "tracing", "config", "secrets", "featureflag", "ratelimit"]],
];

function buildStressServices(count) {
  const flat = STRESS_DOMAINS.flatMap(([domain, kind, names]) =>
    names.map((name) => ({ domain, kind, name })),
  );
  const out = [];
  for (let i = 0; i < count; i++) {
    const pick = flat[i % flat.length];
    const copy = Math.floor(i / flat.length);
    const name = copy === 0 ? pick.name : `${pick.name}-${copy}`;
    const isData = pick.domain === "data" || pick.domain === "infra" || pick.domain === "edge";
    out.push({
      name,
      domain: pick.domain,
      kind: pick.kind,
      ns: isData ? (pick.domain === "edge" ? "edge" : "shop-data") : `shop-${pick.domain}`,
      weight: Math.round((0.3 + ((i * 37) % 13) / 10) * 10) / 10,
      replicas: 1 + ((i * 7) % 6),
      containers: isData ? [name] : ["app", ...(i % 3 === 0 ? ["istio-proxy"] : [])],
      node: `shop-${isData ? "data" : "app"}-${1 + (i % 12)}`,
    });
  }
  return out;
}

function buildStressLinks(services) {
  const byDomain = (d) => services.filter((s) => s.domain === d);
  const links = [];
  const add = (c, s, protocol, w) => {
    if (c && s && c.name !== s.name) links.push({ client: c.name, server: s.name, protocol, weight: w });
  };
  const data = byDomain("data");
  const pickData = (i, offset) => data[(i + offset) % Math.max(1, data.length)];
  for (let i = 0; i < services.length; i++) {
    const s = services[i];
    if (s.domain === "edge") add(s, byDomain("frontend")[i % Math.max(1, byDomain("frontend").length)], "http", 1.2);
    else if (s.domain === "frontend") add(s, byDomain("gateway")[i % Math.max(1, byDomain("gateway").length)], "http", 1.1);
    else if (s.domain === "gateway") {
      const domains = ["identity", "catalog", "commerce", "user", "social"];
      for (let k = 0; k < 3; k++) {
        const pool = byDomain(domains[(i + k) % domains.length]);
        add(s, pool[(i + k) % Math.max(1, pool.length)], k === 1 ? "grpc" : "http", 0.9);
      }
    } else {
      const pool = byDomain(s.domain);
      add(s, pool[(i + 1) % Math.max(1, pool.length)], "grpc", 0.5);
      // Domain-to-domain fan-out for cross-service traffic.
      const order = ["identity", "catalog", "commerce", "fulfillment", "user", "social"];
      const next = order[(order.indexOf(s.domain) + 1 + order.length) % order.length];
      const nextPool = byDomain(next);
      if (nextPool.length) add(s, nextPool[i % nextPool.length], "http", 0.4);
    }
    // Every service talks to the data layer.
    if (s.domain !== "data") {
      add(s, pickData(i, 0), "postgres", 0.6);
      add(s, pickData(i, 2), "redis", 0.5);
      if (i % 2 === 0) add(s, pickData(i, 4), "kafka", 0.3);
    }
  }
  // Outer ingress.
  const edge = byDomain("edge")[0];
  if (edge) links.unshift({ client: "external", server: edge.name, protocol: "http", weight: 1.4 });

  // Deduplicate client/server pairs (one line per workload pair).
  const seen = new Set();
  return links.filter((l) => {
    const key = `${l.client}\0${l.server}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

const SERVICES = isStress
  ? buildStressServices(SERVICE_COUNT)
  : DEMO_SERVICES.slice(0, Math.min(SERVICE_COUNT, DEMO_SERVICES.length));
const LINKS = isStress ? buildStressLinks(SERVICES) : DEMO_LINKS;
const ALL_NODES = [{ name: "external", kind: "external", containers: [], replicas: 0 }, ...SERVICES];
const BY_NAME = new Map(ALL_NODES.map((s) => [s.name, s]));

// ---------------------------------------------------------------------------
// Stack generation
// ---------------------------------------------------------------------------

const DEMO_STACKS = {
  "edge-proxy":    [[["tlsHandshake", "proxyPass", "main"], 0.3], [["httpReadRequest", "proxyPass", "main"], 0.5], [["upstreamConnect", "proxyPass", "main"], 0.2]],
  frontend:        [[["renderCatalog", "fetchAPI", "main"], 0.4], [["renderCart", "fetchAPI", "main"], 0.35], [["hydrate", "main"], 0.25]],
  "api-gateway":   [[["routeMatch", "proxyRequest", "main"], 0.45], [["jwtVerify", "proxyRequest", "main"], 0.3], [["rateLimit", "main"], 0.25]],
  catalog:         [[["queryCatalog", "pgQuery", "main"], 0.45], [["cacheLookup", "redisGet", "main"], 0.35], [["decodeProduct", "main"], 0.2]],
  search:          [[["rankResults", "vectorSearch", "main"], 0.5], [["queryIndex", "esSearch", "main"], 0.35], [["buildFacets", "main"], 0.15]],
  cart:            [[["getCart", "redisGet", "main"], 0.45], [["validateItems", "main"], 0.3], [["mergeCart", "main"], 0.25]],
  checkout:        [[["checkoutCharge", "handleHTTP", "main"], 0.4], [["pgQuery", "checkoutCharge", "handleHTTP", "main"], 0.3], [["reserveStock", "grpcCall", "main"], 0.2], [["validateAddress", "main"], 0.1]],
  order:           [[["createOrder", "pgQuery", "main"], 0.5], [["publishEvent", "kafkaProduce", "main"], 0.3], [["loadOrder", "pgQuery", "main"], 0.2]],
  payment:         [[["chargeCard", "httpCall", "main"], 0.5], [["verifyThreeDS", "httpCall", "main"], 0.3], [["pgQuery", "main"], 0.2]],
  inventory:       [[["reserveStock", "pgQuery", "main"], 0.6], [["getStock", "redisGet", "main"], 0.4]],
  user:            [[["loadSession", "redisGet", "main"], 0.5], [["verifyToken", "main"], 0.3], [["pgQuery", "main"], 0.2]],
  recommendation:  [[["inferRank", "main"], 0.5], [["getFeatures", "redisGet", "main"], 0.3], [["publishClick", "kafkaProduce", "main"], 0.2]],
  review:          [[["loadReviews", "pgQuery", "main"], 0.6], [["indexReview", "esIndex", "main"], 0.4]],
  notification:    [[["consumeEvent", "kafkaConsume", "main"], 0.6], [["sendEmail", "smtpSend", "main"], 0.4]],
  shipping:        [[["getRate", "httpCall", "main"], 0.5], [["createShipment", "grpcCall", "main"], 0.5]],
  postgres:        [[["execQuery", "main"], 0.35], [["indexScan", "execQuery", "main"], 0.3], [["seqScan", "execQuery", "main"], 0.2], [["commitTxn", "main"], 0.15]],
  redis:           [[["getCommand", "main"], 0.5], [["setCommand", "main"], 0.3], [["expireKeys", "main"], 0.2]],
  kafka:           [[["produceBatch", "main"], 0.45], [["consumeBatch", "main"], 0.4], [["rebalance", "main"], 0.15]],
  elasticsearch:   [[["searchShard", "main"], 0.5], [["indexDoc", "main"], 0.35], [["mergeSegments", "main"], 0.15]],
};

// Middle layers for deep stress stacks. Chosen deterministically per (service,
// stack, layer) so a given report has many distinct — but valid — call paths.
const MIDDLE = [
  "validateRequest", "parseBody", "authorize", "loadContext", "beginTx", "applyPolicy",
  "resolveFeature", "buildQuery", "executePlan", "decodeRows", "encodeResponse", "serializeJSON",
  "acquireLock", "awaitFuture", "batchWrite", "flushBuffer", "retryBackoff", "collectMetrics",
  "traceSpan", "checkCache", "fillCache", "mergeResults", "sortResults", "paginate",
  "compressPayload", "signPayload", "verifyChecksum", "routeDispatch", "normalizeInput",
];
const FRAMEWORK = [
  "handler.ServeHTTP", "router.dispatch", "middleware.Chain.Next", "grpc.Server.processUnaryRPC",
  "gin.(*Context).Next", "echo.(*Context).Next", "runtime.goexit", "runtime.main",
  "net/http.(*conn).serve", "net/http.serverHandler.ServeHTTP", "runtime.gopark",
];
const SYS = ["syscall.Syscall", "syscall.futex", "runtime.epollwait", "runtime.netpoll", "internal/poll.(*FD).Read"];

function stackTemplates(service, count, depth) {
  if (!isStress) return DEMO_STACKS[service.name] || [[["work", "main"], 1]];
  const leafNames = Array.from({ length: 14 }, (_, k) => `${service.name}_op${k}`);
  const templates = [];
  for (let s = 0; s < count; s++) {
    const frames = [leafNames[s % leafNames.length]];
    const target = Math.max(3, depth - 6);
    for (let d = 1; d < target; d++) {
      frames.push(MIDDLE[(s * 7 + d * 13 + service.name.length) % MIDDLE.length]);
    }
    // Framework tail, deepest last (leaf-first order).
    const tail = FRAMEWORK.slice((s % 3) * 3, (s % 3) * 3 + 5);
    frames.push(...tail, SYS[s % SYS.length]);
    templates.push([frames, 1]);
  }
  return templates;
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

function traffic(phase) {
  const wave = 0.85 + 0.25 * Math.sin(phase * Math.PI * 2 - 0.6);
  let promo = 1;
  if (phase > 0.5 && phase < 0.78) promo += 1.8 * Math.sin(((phase - 0.5) / 0.28) * Math.PI);
  // A slow weekly rhythm: weekends are busier for a shop.
  const weekly = 1 + 0.2 * Math.sin(phase * Math.PI * 2 * 4);
  return Math.max(0.2, wave * promo * weekly);
}

function jitter(seed) {
  const x = Math.sin(seed * 12.9898) * 43758.5453;
  return 0.9 + (x - Math.floor(x)) * 0.2;
}

function makeReport(service, startNs, endNs, windowIndex) {
  const strings = [""];
  const index = new Map();
  const intern = (value) => {
    let i = index.get(value);
    if (i === undefined) {
      i = strings.length;
      strings.push(value);
      index.set(value, i);
    }
    return i;
  };
  const level = traffic((windowIndex + 0.5) / WINDOWS) * service.weight * jitter(windowIndex + service.name.length);
  const templates = stackTemplates(service, STACKS_PER_REPORT, DEPTH);
  const stacks = templates.map(([frames, share], si) => ({
    frames: frames.map((symbol, fi) => [
      0,
      intern(symbol),
      intern(`/srv/${service.name}/${fi < 3 ? "handler" : "internal"}.go`),
      11 + (si % 7),
    ]),
    value: Math.max(1, Math.round((1_500_000_000 / STACKS_PER_REPORT) * share * level * 8)),
  }));
  const replica = (windowIndex + service.name.length) % Math.max(1, service.replicas);
  return {
    service_name: service.name,
    labels: {
      pod: `${service.name}-${replica}`,
      k8s_pod: `${service.name}-${replica}`,
      k8s_pod_uid: `shop-${service.name}-${replica}`,
      k8s_namespace: service.ns || NS,
      k8s_workload: service.name,
      k8s_workload_kind: service.kind === "service" ? "Service" : "Deployment",
      k8s_container: service.containers[0] || "app",
      k8s_qos_class: "Burstable",
      node: service.node,
      process_name: service.name,
      ...(service.domain ? { shop_domain: service.domain } : {}),
    },
    profile_type: "cpu",
    start_ns: startNs,
    end_ns: endNs,
    sample_rate_hz: 97,
    strings,
    stacks,
  };
}

function makeMapBatch(startNs, endNs, windowIndex) {
  const phase = (windowIndex + 0.5) / WINDOWS;
  const level = traffic(phase);
  const external = {
    id: "external", service: "external", kind: "external",
    rx_bytes: Math.round(30_000 * level), tx_bytes: Math.round(12_000 * level),
    connections: Math.round(40 * level), retransmits: 0, pods: 0, ready: 0,
    instances: [], containers: [],
  };
  const nodes = [external, ...SERVICES.map((s) => {
    const replica = (windowIndex + s.name.length) % Math.max(1, s.replicas);
    const scale = level * s.weight * jitter(windowIndex * 3 + s.name.length);
    const connections = Math.round(40 * scale);
    return {
      id: nodeId(s), namespace: s.ns || NS,
      workload: s.kind === "workload" ? s.name : "",
      service: s.name, kind: s.kind, node: s.node,
      rx_bytes: Math.round(48_000 * scale), tx_bytes: Math.round(24_000 * scale),
      connections, retransmits: Math.round(connections * 0.004),
      pods: s.replicas, ready: s.replicas,
      instances: Array.from({ length: s.replicas }, (_, i) => ({
        name: `${s.name}-${i}`, node: s.node, ready: true, phase: "Running",
      })),
      containers: s.containers,
    };
  })];
  const edges = LINKS.map((link) => {
    const client = BY_NAME.get(link.client);
    const server = BY_NAME.get(link.server);
    const scale = level * link.weight * jitter(windowIndex * 5 + link.client.length);
    const connections = Math.round(60 * scale);
    const samples = Math.max(1, Math.round(6 * scale));
    const sidecar = client?.containers?.includes("istio-proxy");
    return {
      client: client ? nodeId(client) : "external",
      server: server ? nodeId(server) : link.server,
      protocol: link.protocol,
      client_container: sidecar && !["postgres", "redis"].includes(link.protocol) ? "istio-proxy" : client?.containers?.[0] || "",
      server_container: server?.containers?.[0] || "",
      connections, retransmits: Math.round(connections * 0.003),
      tx_bytes: Math.round(12_000 * scale), rx_bytes: Math.round(30_000 * scale),
      rtt_us_sum: Math.round(samples * (800 + 400 * level)), rtt_samples: samples,
    };
  });
  return { start_ns: startNs, end_ns: endNs, node: "shop-sim", nodes, edges };
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

const headers = { "content-type": "application/json", "x-scope-orgid": TENANT };

async function post(path, body, label) {
  if (DRY_RUN) return { accepted: 0 };
  const payload = JSON.stringify(body);
  if (payload.length > 60 * 1024 * 1024) {
    throw new Error(`${label}: request body ${(payload.length / 1e6).toFixed(1)} MB exceeds the 64 MB server limit`);
  }
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const response = await fetch(`${BASE}${path}`, {
        method: "POST", headers, body: payload, signal: AbortSignal.timeout(180_000),
      });
      if (!response.ok) throw new Error(`${label}: HTTP ${response.status} ${(await response.text()).slice(0, 300)}`);
      return await response.json();
    } catch (error) {
      if (attempt === 2) throw error;
      await new Promise((r) => setTimeout(r, 500 * (attempt + 1)));
    }
  }
  return { accepted: 0 };
}

async function getJson(path) {
  const response = await fetch(`${BASE}${path}`, {
    headers: { "x-scope-orgid": TENANT }, signal: AbortSignal.timeout(180_000),
  });
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status} ${(await response.text()).slice(0, 200)}`);
  return response.json();
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

// Pin `now` for a multi-segment run: every segment must derive its boundaries
// from the same instant, otherwise the hours spent writing segment N open a
// gap before segment N+1 and the histogram shows empty buckets at the seams.
const nowNs = Number(arg("now-ms", Date.now())) * 1e6;
// `--until-ms` lets the final segment end at the real current time while the
// earlier segments stay pinned to the shared anchor. Without it, a long run
// would stop at the anchor and the newest hour would stay empty.
const untilMs = Number(arg("until-ms", 0));
const segmentEndNs = untilMs ? untilMs * 1e6 : nowNs - (FROM_HOURS_AGO - HOURS) * HOUR_NS;
const firstNs = segmentEndNs - HOURS * HOUR_NS;
const slot = (i) => ({ start: firstNs + i * WINDOW_NS, end: firstNs + (i + 1) * WINDOW_NS });

console.log(`profile    ${PROFILE}`);
console.log(`base       ${BASE}`);
console.log(`tenant     ${TENANT}`);
console.log(`range      ${new Date(firstNs / 1e6).toISOString()} -> ${new Date(segmentEndNs / 1e6).toISOString()}`);
console.log(`windows    ${WINDOWS} x ${WINDOW_SEC}s = ${(WINDOWS * WINDOW_SEC / 3600).toFixed(2)}h`);
console.log(`nodes      ${SERVICES.length} services + external, ${LINKS.length} edges`);
console.log(`reports    ${WINDOWS * SERVICES.length}; stacks/report ${STACKS_PER_REPORT}, depth ~${DEPTH}`);
console.log(`dry run    ${DRY_RUN ? "yes (nothing will be sent)" : "no"}`);

let sentReports = 0, sentBatches = 0, mapNodes = 0, mapEdges = 0;
const started = Date.now();

for (let i = 0; i < WINDOWS; i += BATCH_WINDOWS) {
  const end = Math.min(i + BATCH_WINDOWS, WINDOWS);
  const reports = [];
  const batches = [];
  for (let w = i; w < end; w++) {
    const { start, end: stop } = slot(w);
    if (!MAP_ONLY) for (const service of SERVICES) reports.push(makeReport(service, start, stop, w));
    batches.push(makeMapBatch(start, stop, w));
  }

  // Profiles: cap each POST both by report count and by an approximate body
  // size. A deep stress report is ~40-80 KB, so 1500 reports stay under 64 MB.
  for (let r = 0; r < reports.length; r += REPORTS_PER_POST) {
    const chunk = reports.slice(r, r + REPORTS_PER_POST);
    const result = await post("/ingest", { reports: chunk }, `ingest windows ${i}..${end - 1}`);
    sentReports += result.accepted ?? chunk.length;
  }
  const mapResult = await post("/api/v1/servicemap/ingest", { batches }, `map windows ${i}..${end - 1}`);
  sentBatches += mapResult.accepted ?? batches.length;
  mapNodes += mapResult.nodes ?? batches.reduce((n, b) => n + b.nodes.length, 0);
  mapEdges += mapResult.edges ?? batches.reduce((n, b) => n + b.edges.length, 0);

  if ((i / BATCH_WINDOWS) % 10 === 0 || end === WINDOWS) {
    const elapsed = ((Date.now() - started) / 1000).toFixed(0);
    console.log(
      `posted     windows ${String(i).padStart(4)}..${String(end - 1).padStart(4)}  ` +
        `${MAP_ONLY ? "map only," : `${reports.length} reports,`} ${batches.length} map deltas  ` +
        `[${sentReports} reports, ${elapsed}s]`,
    );
  }
}
console.log(`written    ${sentReports} reports, ${sentBatches} map batches (${mapNodes} nodes, ${mapEdges} edges) in ${((Date.now() - started) / 1000).toFixed(0)}s`);

// ---------------------------------------------------------------------------
// Verification: read back through the query API at 30-day, daily and hourly
// granularity so each selected time range in the UI is known to return data.
// ---------------------------------------------------------------------------

if (!DRY_RUN && !NO_VERIFY) {
  const slices = [];
  // 30 daily slices (or hourly when the run is shorter than a day).
  const sliceNs = HOURS >= 24 ? 24 * HOUR_NS : HOUR_NS;
  const count = Math.floor(HOURS * HOUR_NS / sliceNs);
  for (let k = 0; k < count; k++) {
    slices.push([firstNs + k * sliceNs + 1e9, Math.min(firstNs + (k + 1) * sliceNs - 1e9, segmentEndNs)]);
  }
  console.log(`\nverifying ${slices.length} time slices...`);
  let ok = 0;
  for (let k = 0; k < slices.length; k++) {
    const [from, until] = slices[k];
    const query = `from=${Math.round(from)}ns&until=${Math.round(until)}ns`;
    // Sample a handful of slices for the full map query; always check flame.
    const flame = await getJson(`/api/v1/flamegraph?${query}`);
    const total = Number(flame.total || 0);
    let nodes = "-", edges = "-";
    if (k % Math.max(1, Math.floor(slices.length / 8)) === 0 || k === slices.length - 1) {
      const map = await getJson(`/api/v1/servicemap?${query}`);
      nodes = map.nodes?.length || 0;
      edges = map.edges?.length || 0;
    }
    const good = total > 0 && (nodes === "-" || (Number(nodes) > 0 && Number(edges) > 0));
    if (good) ok++;
    if (good && k % Math.max(1, Math.floor(slices.length / 10)) !== 0 && k !== slices.length - 1) continue;
    console.log(`${good ? "ok  " : "FAIL"} slice ${String(k).padStart(2)}  flame total=${total}  nodes=${nodes} edges=${edges}`);
  }

  // Deep stacks must actually show up in a flamegraph (many names, deep levels).
  const q = `from=${Math.round(firstNs + HOURS * HOUR_NS / 2)}ns&until=${Math.round(firstNs + HOURS * HOUR_NS / 2 + HOUR_NS)}ns`;
  const deep = await getJson(`/api/v1/flamegraph?${q}`);
  const names = deep.names?.length || 0;
  const maxLevel = Math.max(-1, ...(deep.levels || []));
  console.log(`${names > 20 && maxLevel >= 8 ? "ok  " : "FAIL"} deep stacks: ${names} frame names, max depth level ${maxLevel}`);

  // Every UI-scale bucket must carry traffic. Use the segment's own end as
  // `until`: historical segments end days before `now`, and comparing them
  // against a 30-day domain would leave most buckets empty by construction.
  const buckets = Math.min(240, WINDOWS);
  const histogram = await getJson(
    `/api/v1/servicemap/histogram?from=${Math.round(firstNs)}ns&until=${Math.round(segmentEndNs)}ns&buckets=${buckets}`,
  );
  const nonZero = (histogram.connections || []).filter((v) => v > 0).length;
  const dense = nonZero === buckets;
  console.log(`${dense ? "ok  " : "FAIL"} histogram: ${nonZero}/${buckets} non-empty buckets`);

  if (ok !== slices.length || !dense) {
    console.error(`\nverification failed: ${ok}/${slices.length} slices queryable`);
    process.exitCode = 1;
  } else {
    console.log(`\ndone: ${ok}/${slices.length} slices queryable in tenant "${TENANT}"`);
  }
} else if (!DRY_RUN) {
  console.log("\nverification skipped (--no-verify)");
}
