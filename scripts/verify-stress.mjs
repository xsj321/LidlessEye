#!/usr/bin/env node
// Read-back verification for a 30-day stress seed.
//
// Queries one narrow window per day (instead of scanning 30 days at once) so
// each check answers "can I find data in this time slice" quickly. Also checks
// that deep stacks survive the round trip and that the map is populated.

import process from "node:process";

const BASE = (process.env.LIDLESSEYE_BASE || "http://127.0.0.1:4040").replace(/\/+$/, "");
const TENANT = process.argv[2] || "shop-demo";
const DAYS = Number(process.argv[3] || 30);
const HOUR = 3_600_000_000_000;

const headers = { "x-scope-orgid": TENANT };
async function get(path) {
  const r = await fetch(`${BASE}${path}`, { headers, signal: AbortSignal.timeout(120_000) });
  if (!r.ok) throw new Error(`${path}: HTTP ${r.status} ${(await r.text()).slice(0, 200)}`);
  return r.json();
}

const now = Date.now() * 1e6;
const first = now - DAYS * 24 * HOUR;
let ok = 0;
let deepMax = 0;
let mapChecked = 0;

console.log(`tenant=${TENANT} days=${DAYS} ` +
  `${new Date(first / 1e6).toISOString()} -> ${new Date(now / 1e6).toISOString()}`);

for (let d = 0; d < DAYS; d++) {
  // Noon of each simulated day, one-hour window.
  const from = Math.round(first + d * 24 * HOUR + 12 * HOUR);
  const until = Math.round(from + HOUR);
  const q = `from=${from}ns&until=${until}ns`;
  const flame = await get(`/api/v1/flamegraph?${q}`);
  const total = Number(flame.total || 0);
  const names = flame.names?.length || 0;
  const maxLevel = Math.max(-1, ...(flame.levels || []));
  deepMax = Math.max(deepMax, maxLevel);

  let map = "-";
  if (d % 5 === 0 || d === DAYS - 1) {
    const m = await get(`/api/v1/servicemap?${q}`);
    map = `${m.nodes?.length || 0}n/${m.edges?.length || 0}e`;
    if ((m.nodes?.length || 0) > 0) mapChecked++;
    else map = `FAIL ${map}`;
  }

  const good = total > 0 && !String(map).startsWith("FAIL");
  if (good) ok++;
  console.log(`${good ? "ok  " : "FAIL"} day ${String(d).padStart(2)}  total=${String(total).padStart(13)}  ` +
    `names=${String(names).padStart(4)} depth=${String(maxLevel).padStart(3)}  map=${map}`);
}

// A stress seed is only dense if the histogram is full at every zoom level.
// 120 buckets is the UI default; 1h/6h/24h/30d are the ranges the brush offers.
const BUCKETS = 120;
let histogramOk = true;
for (const [label, hours] of [["1h", 1], ["6h", 6], ["24h", 24], ["30d", DAYS * 24]]) {
  const from = Math.round(now - hours * HOUR);
  const hist = await get(
    `/api/v1/servicemap/histogram?from=${from}ns&until=${Math.round(now)}ns&buckets=${BUCKETS}`,
  );
  const values = hist.connections || [];
  const nonZero = values.filter((v) => v > 0).length;
  const full = nonZero === BUCKETS;
  console.log(`${full ? "ok  " : "FAIL"} histogram ${label}: ${nonZero}/${BUCKETS} non-empty buckets`);
  histogramOk = histogramOk && full;
}
console.log(`deepest flame level: ${deepMax}`);

if (ok === DAYS && histogramOk && deepMax >= 8) {
  console.log(`PASS: ${ok}/${DAYS} days queryable, deep stacks present, histogram dense at all ranges`);
} else {
  console.error(`FAIL: ${ok}/${DAYS} days, histogram dense=${histogramOk}, depth ${deepMax}`);
  process.exitCode = 1;
}
