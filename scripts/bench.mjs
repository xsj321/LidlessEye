#!/usr/bin/env node
// HTTP benchmark for a running LidlessEye server.
//
// It measures three things that matter for a profiling backend:
//
//   sequential  one request at a time, per endpoint — the latency a dashboard
//               sees when it switches views;
//   concurrent  a mixed 1-hour workload at N in-flight requests — throughput
//               and the tail a user feels under load;
//   ranges      the same flamegraph over 1h … 30d — whether latency grows
//               with the scan range or falls off a cliff.
//
// It only talks HTTP; the data is whatever the server already holds. No
// dependencies, so it runs anywhere Node does.
//
// Usage:
//   node scripts/bench.mjs --base http://127.0.0.1:4040 --tenant shop-stress
//   node scripts/bench.mjs --runs 20 --concurrency 32 --duration 20
//   node scripts/bench.mjs --json .tmp/bench.json --markdown .tmp/bench.md
//   node scripts/bench.mjs --quick          # small run for a smoke check
//
// Env: LIDLESSEYE_BASE, LIDLESSEYE_TENANT, LIDLESSEYE_TOKEN.

import process from "node:process";
import { writeFileSync } from "node:fs";

function arg(name, fallback) {
  const flag = `--${name}`;
  const index = process.argv.indexOf(flag);
  if (index < 0) return fallback;
  const value = process.argv[index + 1];
  if (value === undefined || value.startsWith("--")) throw new Error(`${flag} needs a value`);
  return value;
}

const QUICK = process.argv.includes("--quick");
const BASE = (arg("base", process.env.LIDLESSEYE_BASE || "http://127.0.0.1:4040")).replace(/\/+$/, "");
const TENANT = arg("tenant", process.env.LIDLESSEYE_TENANT || "shop-stress");
const TOKEN = arg("token", process.env.LIDLESSEYE_TOKEN || "");
const RUNS = Number(arg("runs", QUICK ? 5 : 15));
const CONCURRENCY = Number(arg("concurrency", QUICK ? 8 : 16));
const DURATION_SEC = Number(arg("duration", QUICK ? 5 : 10));
const JSON_OUT = arg("json", "");
const MARKDOWN_OUT = arg("markdown", "");

const headers = { "x-scope-orgid": TENANT };
if (TOKEN) headers.authorization = `Bearer ${TOKEN}`;

const ENDPOINTS = [
  { name: "stats", path: "/api/v1/stats" },
  { name: "flamegraph 1h", path: "/api/v1/flamegraph?from=now-1h&until=now" },
  { name: "flamegraph 24h", path: "/api/v1/flamegraph?from=now-24h&until=now" },
  { name: "service map 1h", path: "/api/v1/servicemap?from=now-1h&until=now" },
  { name: "map histogram 1h", path: "/api/v1/servicemap/histogram?from=now-1h&until=now&buckets=120" },
  { name: "top 1h", path: "/api/v1/top?from=now-1h&until=now&limit=100" },
  { name: "series 1h", path: "/api/v1/series?from=now-1h&until=now" },
];

const RANGES = ["1h", "6h", "24h", "7d", "30d"];

function percentile(sorted, q) {
  if (sorted.length === 0) return 0;
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(q * sorted.length) - 1));
  return sorted[index];
}

function summarize(name, samples, wallMs) {
  const values = samples.map((s) => s.ms).sort((a, b) => a - b);
  const total = values.reduce((sum, value) => sum + value, 0);
  return {
    name,
    requests: values.length,
    errors: samples.filter((s) => s.error).length,
    bytes: samples.reduce((sum, s) => sum + (s.bytes || 0), 0),
    rps: values.length / (wallMs / 1000),
    mean_ms: total / values.length,
    p50_ms: percentile(values, 0.5),
    p95_ms: percentile(values, 0.95),
    p99_ms: percentile(values, 0.99),
    min_ms: values[0] ?? 0,
    max_ms: values[values.length - 1] ?? 0,
  };
}

async function once(path) {
  const started = performance.now();
  try {
    const response = await fetch(`${BASE}${path}`, {
      headers,
      signal: AbortSignal.timeout(120_000),
    });
    const body = await response.arrayBuffer();
    if (!response.ok) {
      return { ms: performance.now() - started, error: response.status, bytes: body.byteLength };
    }
    return { ms: performance.now() - started, error: 0, bytes: body.byteLength };
  } catch (error) {
    return { ms: performance.now() - started, error: String(error), bytes: 0 };
  }
}

async function sequential(entries, runs) {
  const results = [];
  for (const entry of entries) {
    // One warm-up: the first hit pays for a cold view/footer cache and would
    // otherwise dominate a 5-run sample.
    await once(entry.path);
    const samples = [];
    const started = performance.now();
    for (let i = 0; i < runs; i++) samples.push(await once(entry.path));
    results.push(summarize(entry.name, samples, performance.now() - started));
  }
  return results;
}

async function concurrent(entries, concurrency, durationSec) {
  const deadline = performance.now() + durationSec * 1000;
  const samples = [];
  let cursor = 0;
  const started = performance.now();

  async function worker() {
    while (performance.now() < deadline) {
      const entry = entries[cursor++ % entries.length];
      samples.push(await once(entry.path));
    }
  }
  await Promise.all(Array.from({ length: concurrency }, worker));
  return summarize(`mixed 1h × ${concurrency}`, samples, performance.now() - started);
}

async function ranges(runs) {
  const out = [];
  for (const range of RANGES) {
    const entry = {
      name: `flamegraph ${range}`,
      path: `/api/v1/flamegraph?from=now-${range}&until=now`,
    };
    await once(entry.path);
    const samples = [];
    const started = performance.now();
    for (let i = 0; i < runs; i++) samples.push(await once(entry.path));
    out.push(summarize(entry.name, samples, performance.now() - started));
  }
  return out;
}

function table(rows) {
  const head = "| Endpoint | Requests | RPS | p50 | p95 | p99 | Max | Errors |";
  const rule = "|---|---:|---:|---:|---:|---:|---:|---:|";
  const body = rows.map(
    (row) =>
      `| ${row.name} | ${row.requests} | ${row.rps.toFixed(1)} | ` +
      `${row.p50_ms.toFixed(1)} ms | ${row.p95_ms.toFixed(1)} ms | ${row.p99_ms.toFixed(1)} ms | ` +
      `${row.max_ms.toFixed(1)} ms | ${row.errors} |`,
  );
  return [head, rule, ...body].join("\n");
}

function fmt(value) {
  return `${value.toFixed(1)} ms`;
}

const startedAt = new Date();
console.log(`base       ${BASE}`);
console.log(`tenant     ${TENANT}`);
console.log(`runs       ${RUNS}, concurrency ${CONCURRENCY}, duration ${DURATION_SEC}s`);
console.log(`started    ${startedAt.toISOString()}\n`);

const seq = await sequential(ENDPOINTS, RUNS);
console.log("sequential\n" + table(seq) + "\n");

// The concurrent scenario is the background polling a dashboard generates:
// stats, map, histogram, top and series. A flamegraph is a deliberate click,
// not a poll, and mixing a 30-day scan into this loop would measure queueing
// behind the longest query instead of UI concurrency.
const CONCURRENT_ENDPOINTS = ENDPOINTS.filter((entry) =>
  ["stats", "service map 1h", "map histogram 1h", "top 1h", "series 1h"].includes(entry.name),
);
const conc = await concurrent(CONCURRENT_ENDPOINTS, CONCURRENCY, DURATION_SEC);
conc.name = `dashboard polling × ${CONCURRENCY}`;
console.log(
  `concurrent (${CONCURRENT_ENDPOINTS.map((entry) => entry.name).join(", ")})\n` +
    table([conc]) +
    "\n",
);

const scan = await ranges(QUICK ? 3 : 8);
console.log("scan ranges\n" + table(scan) + "\n");

const report = {
  generated_at: startedAt.toISOString(),
  base: BASE,
  tenant: TENANT,
  config: { runs: RUNS, concurrency: CONCURRENCY, duration_sec: DURATION_SEC },
  sequential: seq,
  concurrent: conc,
  ranges: scan,
};

if (JSON_OUT) writeFileSync(JSON_OUT, `${JSON.stringify(report, null, 2)}\n`);

if (MARKDOWN_OUT) {
  const md = [
    `Generated ${report.generated_at}`,
    "",
    "**Sequential**",
    "",
    table(seq),
    "",
    "**Concurrent**",
    "",
    table([conc]),
    "",
    "**Scan range**",
    "",
    table(scan),
    "",
  ].join("\n");
  writeFileSync(MARKDOWN_OUT, md);
}

console.log(`p95 summary: ${seq.map((row) => `${row.name}=${fmt(row.p95_ms)}`).join(", ")}`);
