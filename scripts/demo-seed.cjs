// Seed a running LidlessEye server with a demo dataset so the dashboard has
// something real to draw: several services, 45 one-minute windows, cpu and
// off_cpu types, two tenants.
//
//   BASE=http://127.0.0.1:4040 node demo-seed.cjs
//
// `scripts/demo.sh` starts a server and runs this; the file is standalone so
// it can also seed an instance that is already running.
//
// Values are nanoseconds of CPU (or off-CPU) time per minute, not sample counts.

const BASE = (process.env.BASE || "http://127.0.0.1:4040").replace(/\/+$/, "");
const MINUTES = Number(process.env.MINUTES || 45);

/** Deterministic 0.75–1.25 jitter so the time series is not a flat line. */
function jitter(minute, seed) {
  const x = Math.sin((minute + 1) * 12.9898 + seed * 78.233) * 43758.5453;
  return 0.75 + (x - Math.floor(x)) * 0.5;
}

/**
 * A report in the columnar wire format: one shared string table, each stack a
 * list of frame refs `[module, symbol, file, line]`, leaf first.
 */
function report({ service, labels, profileType, startNs, endNs, strings, stacks }) {
  const intern = (value) => {
    const existing = strings.indexOf(value);
    if (existing >= 0) return existing;
    strings.push(value);
    return strings.length - 1;
  };
  const encoded = stacks.map(([frames, value]) => ({
    frames: frames.map((symbol) => [0, intern(symbol), intern(`/src/${symbol}.rs`), 7]),
    value: Math.round(value),
  }));
  return {
    service_name: service,
    labels,
    profile_type: profileType,
    start_ns: startNs,
    end_ns: endNs,
    sample_rate_hz: profileType === "cpu" ? 99 : 997,
    strings,
    stacks: encoded,
  };
}

async function ingest(tenant, reports) {
  const response = await fetch(`${BASE}/ingest`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-scope-orgid": tenant },
    body: JSON.stringify({ reports }),
  });
  if (!response.ok) {
    throw new Error(`ingest into ${tenant} failed: ${response.status} ${await response.text()}`);
  }
  return response.json();
}

// Call trees as [stack, nanoseconds]. A stack is the symbols from leaf to root.
const TREES = {
  acme: {
    checkout: {
      cpu: [
        [["checkoutCharge", "handleHTTP", "run", "main"], 480e6],
        [["pgSendQuery", "pgQuery", "checkoutCharge", "handleHTTP", "run", "main"], 90e6],
        [["pgParseResult", "pgQuery", "checkoutCharge", "handleHTTP", "run", "main"], 120e6],
        [["tlsHandshake", "stripeCharge", "checkoutCharge", "handleHTTP", "run", "main"], 70e6],
        [["h2FrameWrite", "stripeCharge", "checkoutCharge", "handleHTTP", "run", "main"], 60e6],
        [["serdeSerialize", "jsonEncode", "checkoutCharge", "handleHTTP", "run", "main"], 45e6],
        [["writeAll", "jsonEncode", "checkoutCharge", "handleHTTP", "run", "main"], 15e6],
        [["checkoutValidate", "handleHTTP", "run", "main"], 190e6],
        [["validateCart", "checkoutValidate", "handleHTTP", "run", "main"], 110e6],
        [["sha2Update", "validateCart", "checkoutValidate", "handleHTTP", "run", "main"], 25e6],
        [["sessionLookup", "checkoutValidate", "handleHTTP", "run", "main"], 55e6],
        [["readSession", "sessionLookup", "checkoutValidate", "handleHTTP", "run", "main"], 55e6],
        [["parseBody", "handleHTTP", "run", "main"], 75e6],
        [["parseJson", "parseBody", "handleHTTP", "run", "main"], 40e6],
        [["hyperPoll", "handleHTTP", "run", "main"], 120e6],
        [["runtime.gc", "main"], 40e6],
      ],
      off_cpu: [
        [["pgWait", "pgQuery", "checkoutCharge", "handleHTTP", "run", "main"], 1800e6],
        [["tcpPoll", "h2FrameWrite", "stripeCharge", "checkoutCharge", "handleHTTP", "run", "main"], 900e6],
        [["mutexWait", "sessionLookup", "checkoutValidate", "handleHTTP", "run", "main"], 400e6],
      ],
    },
    search: {
      cpu: [
        [["searchQuery", "handleHTTP", "run", "main"], 380e6],
        [["ftsScan", "searchQuery", "handleHTTP", "run", "main"], 160e6],
        [["mmapRead", "ftsScan", "searchQuery", "handleHTTP", "run", "main"], 90e6],
        [["postingsDecode", "ftsScan", "searchQuery", "handleHTTP", "run", "main"], 70e6],
        [["rrfMerge", "searchQuery", "handleHTTP", "run", "main"], 55e6],
        [["renderJSON", "handleHTTP", "run", "main"], 70e6],
        [["serdeSerialize", "renderJSON", "handleHTTP", "run", "main"], 60e6],
      ],
      off_cpu: [
        [["diskRead", "mmapRead", "ftsScan", "searchQuery", "handleHTTP", "run", "main"], 1200e6],
        [["channelRecv", "handleHTTP", "run", "main"], 350e6],
      ],
    },
    cart: {
      cpu: [
        [["cartGet", "handleHTTP", "run", "main"], 150e6],
        [["redisGet", "cartGet", "handleHTTP", "run", "main"], 85e6],
        [["respParse", "redisGet", "cartGet", "handleHTTP", "run", "main"], 30e6],
        [["cartUpdate", "handleHTTP", "run", "main"], 120e6],
        [["redisSet", "cartUpdate", "handleHTTP", "run", "main"], 70e6],
        [["respParse", "redisSet", "cartUpdate", "handleHTTP", "run", "main"], 25e6],
      ],
      off_cpu: [[["redisWait", "redisGet", "cartGet", "handleHTTP", "run", "main"], 600e6]],
    },
  },
  globex: {
    billing: {
      cpu: [
        [["invoiceRun", "cron", "main"], 700e6],
        [["sqlCopy", "invoiceRun", "cron", "main"], 300e6],
        [["pdfRender", "invoiceRun", "cron", "main"], 220e6],
        [["fontSubset", "pdfRender", "invoiceRun", "cron", "main"], 90e6],
      ],
      off_cpu: [[["fsyncWait", "sqlCopy", "invoiceRun", "cron", "main"], 1500e6]],
    },
  },
};

const LABELS = {
  acme: { checkout: { pod: "checkout-1", region: "eu-west", version: "1.4.2" },
          search: { pod: "search-1", region: "eu-west", version: "1.4.2" },
          cart: { pod: "cart-1", region: "us-east", version: "1.4.1" } },
  globex: { billing: { pod: "billing-1", region: "us-east", version: "2.0.0" } },
};

async function main() {
  const nowNs = Date.now() * 1e6;
  const windowNs = 60_000_000_000;

  for (const [tenant, services] of Object.entries(TREES)) {
    for (const [service, byType] of Object.entries(services)) {
      for (const [profileType, trees] of Object.entries(byType)) {
        const reports = [];
        for (let minute = 0; minute < MINUTES; minute++) {
          const endNs = nowNs - (MINUTES - 1 - minute) * windowNs;
          const stacks = trees.map(([frames, base], i) => [
            frames,
            base * jitter(minute, i + service.length),
          ]);
          reports.push(
            report({
              service,
              labels: LABELS[tenant][service],
              profileType,
              startNs: endNs - windowNs,
              endNs,
              strings: [""],
              stacks,
            }),
          );
        }
        const result = await ingest(tenant, reports);
        console.log(
          `seeded ${tenant}/${service}/${profileType}: ` +
            `${result.reports ?? reports.length} reports, ${result.bytes ?? "?"} bytes`,
        );
      }
    }
  }

  const types = await (await fetch(`${BASE}/api/v1/profile_types`, {
    headers: { "x-scope-orgid": "acme" },
  })).json();
  console.log("profile types:", JSON.stringify(types));
  const stats = await (await fetch(`${BASE}/api/v1/stats`, {
    headers: { "x-scope-orgid": "acme" },
  })).json();
  console.log("stats:", JSON.stringify(stats));
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
