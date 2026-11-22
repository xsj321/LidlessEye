// Browser smoke test for the embedded dashboard.
//
// The Rust suites cover the API; this covers what a human actually clicks:
// every tab renders, the flamegraph paints real pixels, the histogram brush
// drives an absolute time window, the compare view brushes two windows, the
// label explorer and the structured filter rows write into the selector, and
// switching tenant really switches data.
//
// Run it through `scripts/ui-smoke.sh`, which starts a server, seeds two
// tenants and then runs this file inside the Playwright image. Requirements:
// `playwright` must resolve, `LIDLESSEYE_BASE` points at the server, and
// `SHOT_DIR` (optional) is where screenshots land.
//
// CommonJS on purpose: the image provides Playwright on `NODE_PATH`, which ESM
// imports ignore.

const fs = require("node:fs");
const { chromium } = require("playwright");

const BASE = (process.env.LIDLESSEYE_BASE || "http://127.0.0.1:4040").replace(/\/+$/, "");
const SHOTS = process.env.SHOT_DIR || "";

const failures = [];
const consoleErrors = [];
// Kept for the failure dump: an empty view is usually a request problem.
const apiCalls = [];

function check(ok, message, detail) {
  if (!ok) {
    failures.push(detail ? `${message} :: ${detail}` : message);
  }
  console.log(`${ok ? "ok  " : "FAIL"} ${message}`);
}

// Mirrors ROW_HEIGHT in lib/canvas/flamegraph.ts: hit tests are row-based.
const ROW = 22;

/** `2026-11-12T20:15:00` in local time, the value <input type=datetime-local> takes. */
function localInput(date) {
  const pad = (n) => String(n).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(
    date.getHours(),
  )}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

/** Wait until the flamegraph stops moving (load-in or zoom transition). */
async function settle(page, timeout = 4000) {
  await page
    .waitForFunction(
      () => !document.querySelector("canvas.fg-canvas")?.dataset.motion,
      null,
      { timeout },
    )
    .catch(() => {});
}

/** Wait until the server answers, so a slow start is not a test failure. */
async function waitForServer(deadlineMs = 20000) {
  const deadline = Date.now() + deadlineMs;
  let lastError = "";
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${BASE}/api/v1/health`);
      if (response.ok) return;
      lastError = `HTTP ${response.status}`;
    } catch (error) {
      lastError = error.message;
    }
    await new Promise((resolve) => setTimeout(resolve, 200));
  }
  throw new Error(`${BASE} never became healthy: ${lastError}`);
}

/**
 * A report in the columnar wire format: one string table, stacks of frame refs
 * `[module, symbol, file, line]`. Leaf first, like the kernel reports it.
 */
function report({ service, pod, stacks, fromNs, toNs }) {
  const strings = [""];
  const intern = (value) => {
    const existing = strings.indexOf(value);
    if (existing >= 0) return existing;
    strings.push(value);
    return strings.length - 1;
  };
  const encoded = stacks.map(([frames, value]) => ({
    frames: frames.map((symbol) => [0, intern(symbol), intern(`/src/${symbol}.rs`), 7]),
    value,
  }));
  return {
    service_name: service,
    labels: pod ? { pod } : {},
    profile_type: "cpu",
    start_ns: fromNs,
    end_ns: toNs,
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
}

/** A small topology so the map and its node panel have something to open. */
async function ingestMap(tenant, batch) {
  const response = await fetch(`${BASE}/api/v1/servicemap/ingest`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-scope-orgid": tenant },
    body: JSON.stringify(batch),
  });
  if (!response.ok) {
    throw new Error(`service map ingest into ${tenant} failed: ${response.status} ${await response.text()}`);
  }
}

/** Prove the seed is queryable before blaming the UI for an empty view. */
async function expectData(tenant) {
  const response = await fetch(`${BASE}/api/v1/flamegraph?from=now-1h`, {
    headers: { "x-scope-orgid": tenant },
  });
  const body = await response.json();
  check(
    response.ok && body.total > 0,
    `seeded data is queryable for tenant ${tenant}`,
    JSON.stringify(body).slice(0, 200),
  );
}

/** Two tenants, each with a function name no other tenant has. */
async function seed() {
  const nowNs = Date.now() * 1e6;
  const minute = 60_000_000_000;
  await ingest("acme", [
    // A batch half an hour old: gives the histogram brush a second peak and
    // a window the drag test can select without touching the recent one.
    report({
      service: "checkout",
      pod: "checkout-1",
      fromNs: nowNs - 30 * minute,
      toNs: nowNs - 29 * minute,
      stacks: [
        [["oldCheckout", "handleHTTP", "main"], 800_000_000],
        [["oldCharge", "oldCheckout", "handleHTTP", "main"], 300_000_000],
      ],
    }),
    report({
      service: "checkout",
      pod: "checkout-1",
      fromNs: nowNs - minute,
      toNs: nowNs,
      stacks: [
        [["checkoutCharge", "handleHTTP", "main"], 500_000_000],
        [["checkoutValidate", "handleHTTP", "main"], 200_000_000],
        [["pgQuery", "checkoutCharge", "handleHTTP", "main"], 120_000_000],
      ],
    }),
  ]);
  // A ClusterIP node's profiles are selected by `service_name`, which is what
  // the seeded reports carry — the node panel's embedded flamegraph uses it.
  await ingestMap("acme", {
    // Keep the bucket clear of the `now-1h` boundary: storage buckets by the
    // minute and a bucket starting exactly at `from` is excluded.
    start_ns: nowNs - 30 * minute,
    end_ns: nowNs,
    node: "smoke-host",
    nodes: [
      {
        id: "checkout",
        service: "checkout",
        kind: "service",
        connections: 240,
        retransmits: 2,
        rx_bytes: 48_000,
        tx_bytes: 24_000,
      },
      // A workload with a sidecar: the node's title must be the workload name,
      // and the panel must give each container its own row.
      {
        id: "shop/cart",
        namespace: "shop",
        workload: "cart",
        service: "cart",
        kind: "workload",
        pods: 1,
        ready: 1,
        containers: ["app", "istio-proxy"],
      },
      {
        id: "shop/payments",
        namespace: "shop",
        workload: "payments",
        service: "payments",
        kind: "workload",
        pods: 1,
        ready: 1,
        containers: ["app"],
      },
      { id: "external", service: "external", kind: "external" },
    ],
    edges: [
      {
        client: "external",
        server: "checkout",
        protocol: "http",
        connections: 240,
        retransmits: 2,
        rx_bytes: 48_000,
        tx_bytes: 24_000,
        rtt_us_sum: 36_000,
        rtt_samples: 12,
      },
      {
        client: "shop/cart",
        server: "checkout",
        protocol: "http",
        client_container: "istio-proxy",
        connections: 120,
        retransmits: 0,
        rx_bytes: 12_000,
        tx_bytes: 6_000,
        rtt_us_sum: 24_000,
        rtt_samples: 12,
      },
      {
        client: "external",
        server: "shop/cart",
        protocol: "http",
        connections: 30,
        retransmits: 0,
        rx_bytes: 3_000,
        tx_bytes: 1_500,
        rtt_us_sum: 5_000,
        rtt_samples: 5,
      },
      // Two callees of one node that also talk to each other: the neighbourhood
      // draws that as an arc outside the callee column.
      {
        client: "checkout",
        server: "shop/payments",
        protocol: "http",
        connections: 20,
        retransmits: 0,
        rx_bytes: 2_000,
        tx_bytes: 1_000,
        rtt_us_sum: 4_000,
        rtt_samples: 4,
      },
      {
        client: "shop/cart",
        server: "shop/payments",
        protocol: "grpc",
        client_container: "app",
        server_container: "app",
        connections: 60,
        retransmits: 0,
        rx_bytes: 9_000,
        tx_bytes: 30_000,
        rtt_us_sum: 6_000,
        rtt_samples: 6,
      },
    ],
  });
  await ingest("zeta", [
    report({
      service: "search",
      pod: "search-1",
      fromNs: nowNs - minute,
      toNs: nowNs,
      stacks: [
        [["zetaSearch", "handleHTTP", "main"], 300_000_000],
        [["zetaRank", "zetaSearch", "handleHTTP", "main"], 90_000_000],
      ],
    }),
  ]);
}

/** The highlighted tab must name the view that is actually rendered. */
async function checkActiveTab(page, expected) {
  const active = await page.locator('#tabs [data-view][data-state="active"]').evaluateAll((links) =>
    links.map((link) => link.dataset.view),
  );
  check(
    active.length === 1 && active[0] === expected,
    `the ${expected} tab is the only one highlighted`,
    JSON.stringify(active),
  );
}

/**
 * Pick an option from a Radix Select.
 *
 * The toolbar uses shadcn's Select, whose trigger is a button rather than a
 * native <select>, so `selectOption` does not apply: open the trigger, then
 * click the option by its visible text.
 */
async function choose(page, trigger, optionText) {
  await page.click(trigger);
  await page.getByRole("option", { name: optionText, exact: true }).first().click();
  await page.waitForTimeout(50);
}

async function shot(page, name) {
  if (!SHOTS) return;
  // Park the pointer first: a hover style left over from the last click looks
  // like a second highlighted tab in the screenshot.
  await page.mouse.move(0, 0);
  await page.screenshot({ path: `${SHOTS}/${name}.png`, fullPage: true });
}

let page = null;

async function main() {
  await waitForServer();
  await seed();
  await expectData("acme");
  await expectData("zeta");

  const browser = await chromium.launch({
    // `--no-sandbox`: the container does not grant the privileges Chromium's
    // sandbox needs. `--disable-dev-shm-usage`: the container's /dev/shm is
    // 64 MB by default and a renderer that exhausts it dies silently, leaving
    // a blank page with no failed requests — which looks exactly like a broken
    // application.
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  page = await context.newPage();
  page.on("console", (message) => {
    if (message.type() === "error") consoleErrors.push(message.text());
  });
  page.on("pageerror", (error) => consoleErrors.push(`pageerror: ${error.message}`));
  page.on("requestfailed", (request) => {
    // Switching node or tab aborts a fetch that is still in flight; the
    // browser reports that as ERR_ABORTED. A cancelled request is not a
    // failure, so it must not fail the run — a real network error still does.
    const errorText = request.failure()?.errorText ?? "";
    if (errorText.includes("ERR_ABORTED")) return;
    consoleErrors.push(`request failed: ${request.url()} ${errorText}`);
  });
  page.on("response", (response) => {
    if (response.url().includes("/api/")) apiCalls.push(`${response.status()} ${response.url()}`);
  });

  // ---- the front door --------------------------------------------------
  // A bare URL is what people get when they bookmark the host itself. It must
  // land on the map, not on one function in one profile.
  const rootPage = await context.newPage();
  await rootPage.goto(`${BASE}/`, { waitUntil: "load" });
  await rootPage.waitForSelector('#tabs [data-view]', { timeout: 20000 });
  const landing = await rootPage
    .locator('#tabs [data-view][data-state="active"]')
    .evaluateAll((links) => links.map((link) => link.dataset.view));
  check(
    landing.length === 1 && landing[0] === "servicemap",
    "a bare URL opens the service map",
    JSON.stringify(landing),
  );
  await rootPage.close();

  // A link straight into the compare tab has to prepare the two windows the
  // same way the tab click does. It used to leave the comparison window as the
  // whole domain and the baseline as its first half, which is a diff of a span
  // against a part of itself.
  const comparePage = await context.newPage();
  await comparePage.goto(`${BASE}/#/compare?type=cpu&range=1h`, { waitUntil: "load" });
  await comparePage.waitForFunction(
    () => window.lidlesseye?.state?.view === "compare" && window.lidlesseye.state.ready,
    null,
    { timeout: 20000 },
  );
  const direct = await comparePage.evaluate(() => ({
    domainFrom: window.lidlesseye.state.domainFrom,
    domainUntil: window.lidlesseye.state.domainUntil,
    from: window.lidlesseye.state.fromNs,
    until: window.lidlesseye.state.untilNs,
    baseFrom: window.lidlesseye.state.baseFromNs,
    baseUntil: window.lidlesseye.state.baseUntilNs,
  }));
  check(
    direct.from > direct.domainFrom &&
      direct.until >= direct.domainUntil &&
      direct.baseFrom >= direct.domainFrom &&
      direct.baseUntil <= direct.from,
    "a compare link splits the domain instead of overlapping itself",
    JSON.stringify(direct),
  );
  await comparePage.close();

  // ---- flamegraph ------------------------------------------------------
  const navigation = await page.goto(`${BASE}/#/flamegraph`, { waitUntil: "load" });
  check(navigation !== null && navigation.status() === 200, "the dashboard responds with 200", String(navigation && navigation.status()));
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });

  check((await page.title()) === "LidlessEye", "page title");
  check((await page.locator("#tabs [data-view]").count()) === 7, "seven tabs are rendered");
  check(
    (await page.locator("header #tenant").count()) === 1,
    "the tenant selector lives in the header bar",
  );

  const box = await page.locator("canvas.fg-canvas").boundingBox();
  check(box !== null && box.width > 100 && box.height > 100, "flamegraph canvas has a size", JSON.stringify(box));

  // The bars grow in on load and every zoom interpolates its viewport. The
  // canvas marks itself with `data-motion` while it is moving, so the checks
  // below can wait for a still frame instead of racing the animation.
  const introMotion = await page.evaluate(
    () => document.querySelector("canvas.fg-canvas")?.dataset.motion ?? null,
  );
  check(introMotion === "intro", "the flamegraph animates into place", String(introMotion));
  await settle(page);

  // A click selects the frame and opens its details; it must not move the
  // viewport, because a pane that keeps re-laying out is what made the details
  // unreachable before. Row 3 at a quarter of the width is a wide frame in this
  // seed no matter how the old batch reorders the siblings.
  const frameX = box.x + box.width * 0.25;
  const frameY = box.y + ROW * 3 + ROW / 2;
  await page.mouse.click(frameX, frameY);
  await settle(page);
  check(
    (await page.locator('[data-testid="frame-details"]').count()) === 1,
    "clicking a frame opens its details",
  );
  check(
    (await page.evaluate(
      () => document.querySelector("canvas.fg-canvas")?.dataset.motion ?? null,
    )) === null,
    "selecting a frame does not move the viewport",
  );
  const frameName = await page
    .locator('[data-testid="frame-details"] p')
    .first()
    .innerText();
  check(frameName.length > 0, "the details name the frame", frameName);

  // The details report the count that the payload carries per node, not a
  // value derived from the duration: the two answer different questions.
  const detailStats = await page.locator('[data-testid="frame-details"] dl dt').allInnerTexts();
  check(
    detailStats.map((text) => text.toLowerCase()).includes("samples"),
    "the details report the frame's own sample count",
    JSON.stringify(detailStats),
  );

  // The breadcrumb always names the level, so "how deep am I" has an answer
  // even before the first zoom.
  const rootCrumb = (await page.locator('[data-role="crumb-root"]').innerText()).trim();
  check(/all frames/i.test(rootCrumb), "the breadcrumb names the root", rootCrumb);
  await shot(page, "1a-frame-details");

  // Double click zooms, and the breadcrumb carries the way back.
  await page.mouse.dblclick(frameX, frameY);
  const zoomMotion = await page.evaluate(
    () => document.querySelector("canvas.fg-canvas")?.dataset.motion ?? null,
  );
  check(zoomMotion === "zoom", "double clicking a frame starts a viewport transition", String(zoomMotion));
  await settle(page);
  const zoomNote = await page.locator('.fg-legend [data-role="zoom"]').innerText().catch(() => "");
  check(/% of total shown/.test(zoomNote), "the zoom landed on the clicked frame", zoomNote);

  await page.keyboard.press("Escape");
  await settle(page);
  check(
    (await page.locator('[data-testid="frame-details"]').count()) === 0,
    "Escape drops the selection first",
  );
  await page.keyboard.press("Escape");
  await settle(page);
  check(
    (await page.locator('.fg-legend [data-role="zoom"]').count()) === 0,
    "Escape animates back to the whole graph",
  );

  // The toolbar can zoom without a pointer round trip, and the export menu
  // offers the formats a report can carry.
  await page.click('[aria-label="Zoom in"]');
  await settle(page);
  check(
    (await page.locator('.fg-legend [data-role="zoom"]').count()) === 1,
    "the toolbar zooms in",
  );
  await page.click('[aria-label="Reset zoom"]');
  await settle(page);
  check(
    (await page.locator('.fg-legend [data-role="zoom"]').count()) === 0,
    "reset returns to the whole graph",
  );

  await page.click("#flame-export");
  const exportItems = await page
    .locator('[data-slot="popover-content"] button')
    .allInnerTexts();
  check(
    exportItems.some((text) => /SVG/.test(text)) && exportItems.some((text) => /JSON/.test(text)),
    "export offers SVG and JSON",
    JSON.stringify(exportItems),
  );

  // The exports are documents, not an empty promise: take the download the
  // menu triggers and read what actually landed on disk.
  const [svgDownload] = await Promise.all([
    page.waitForEvent("download"),
    page.click('[data-slot="popover-content"] button:has-text("SVG")'),
  ]);
  const svg = fs.readFileSync(await svgDownload.path(), "utf8").trim();
  check(
    svg.startsWith("<svg") && svg.endsWith("</svg>") && (svg.match(/<rect /g) || []).length > 1,
    "the SVG export is a complete document with frames",
    `${svg.length} bytes`,
  );

  await page.click("#flame-export");
  const [jsonDownload] = await Promise.all([
    page.waitForEvent("download"),
    page.click('[data-slot="popover-content"] button:has-text("JSON")'),
  ]);
  const exported = JSON.parse(fs.readFileSync(await jsonDownload.path(), "utf8"));
  check(
    Array.isArray(exported.payload?.names) &&
      typeof exported.view?.x0 === "number" &&
      Array.isArray(exported.crumbs),
    "the JSON export carries the payload and the view",
    Object.keys(exported).join(","),
  );

  await shot(page, "1-flamegraph");

  // ---- navigation drawer and settings ----------------------------------
  await page.click("#sidebar-toggle");
  check(
    (await page.locator("#sidebar").getAttribute("data-collapsed")) === "true",
    "the navigation drawer collapses",
  );
  check(
    (await page.locator("#tabs [data-view]").count()) === 7,
    "collapsing keeps every view reachable",
  );
  check(
    (await page.locator("#tabs #sidebar-toggle").count()) === 1,
    "the expand button is the last option of the collapsed rail",
  );
  await page.click('#tabs [data-view="top"]');
  await checkActiveTab(page, "top");
  await page.click("#sidebar-toggle");
  check(
    (await page.locator("#sidebar").getAttribute("data-collapsed")) === "false",
    "the navigation drawer expands again",
  );
  check(
    (await page.locator("#tabs #sidebar-toggle").count()) === 0 &&
      (await page.locator("#sidebar-toggle").count()) === 1,
    "the toggle leaves the icon rail when expanded and no longer overlaps the brand",
  );

  await page.click("#settings");
  await page.waitForSelector('[data-testid="settings-dialog"]', { timeout: 10000 });
  check(
    await page.locator("#settings-default-view").isVisible(),
    "settings exposes the start-up preferences",
  );
  await page.click("#settings-edge-labels");
  const storedSettings = await page.evaluate(() => window.localStorage.getItem("lidlesseye.settings"));
  check(
    Boolean(storedSettings) && JSON.parse(storedSettings).edgeLabels === false,
    "a settings change is persisted in this browser",
    String(storedSettings),
  );
  await page.click("#settings-edge-labels");
  await page.keyboard.press("Escape");

  await page.click("#histogram-toggle");
  check(
    (await page.locator("#time-histogram").getAttribute("data-collapsed")) === "true",
    "the histogram collapses",
  );
  check(
    (await page.locator("#histogram-toggle").getAttribute("aria-expanded")) === "false",
    "the toggle reports the collapsed state",
  );
  await page.click("#histogram-toggle");
  check(
    (await page.locator("#time-histogram").getAttribute("data-collapsed")) === "false",
    "the histogram expands again",
  );

  await page.click('#tabs [data-view="flamegraph"]');
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });
  await checkActiveTab(page, "flamegraph");

  // The tenant selector is only shown when more than one tenant exists.
  const initialTenant = (await page.locator("#tenant").innerText()).trim();
  check(initialTenant === "acme", "the tenant selector defaults to the first tenant", initialTenant);
  check(!(await page.locator("#tenant").isHidden()), "tenant selector is visible with two tenants");

  // ---- top functions ---------------------------------------------------
  await page.click('#tabs [data-view="top"]');
  await page.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });
  await checkActiveTab(page, "top");
  let topText = await page.locator(".tbl-wrap table tbody").innerText();
  check(topText.includes("checkoutCharge"), "top table lists acme's functions", topText.slice(0, 200));
  check(!topText.includes("zetaSearch"), "top table does not leak another tenant", topText.slice(0, 200));
  await shot(page, "2-top");

  // ---- time series -----------------------------------------------------
  await page.click('#tabs [data-view="timeseries"]');
  await page.waitForSelector("main canvas", { timeout: 20000 });
  await checkActiveTab(page, "timeseries");
  check((await page.locator("main canvas").count()) >= 1, "time series renders a chart");
  await shot(page, "3-timeseries");

  // ---- absolute time range + histogram brush ---------------------------
  // The picker decides the domain first; the bracket then slides inside it.
  // Pick 40 minutes (the seed spans the last 30 plus a batch at now-30m), then
  // bracket the older batch: whatever the window covers is what every view
  // below queries.
  await page.waitForSelector("canvas.time-brush", { timeout: 20000 });
  check(
    (await page
      .locator('#range-mode [data-mode="relative"]')
      .getAttribute("data-state")) === "on",
    "relative is the initial range mode",
  );
  const pickNow = Date.now();
  const pickFrom = new Date(pickNow - 40 * 60_000);
  const pickUntil = new Date(pickNow - 5_000);
  await page.click('#range-mode [data-mode="absolute"]');
  await page.waitForFunction(() => document.getElementById("abs-range") !== null, null, {
    timeout: 5000,
  });
  await page.fill("#abs-from", localInput(pickFrom));
  await page.fill("#abs-until", localInput(pickUntil));
  await page.click("#abs-apply");
  await page.waitForFunction(() => window.lidlesseye.state.mode === "absolute", null, {
    timeout: 10000,
  });
  const domain = await page.evaluate(() => ({
    from: window.lidlesseye.state.domainFrom,
    until: window.lidlesseye.state.domainUntil,
  }));
  check(
    Math.abs(domain.from / 1e6 - pickFrom.getTime()) < 2000 &&
      Math.abs(domain.until / 1e6 - pickUntil.getTime()) < 2000,
    "the date-time inputs define the domain",
    `${JSON.stringify(domain)} vs ${pickFrom.toISOString()}`,
  );
  check((await page.url()).includes("from="), "the absolute domain is in the URL", page.url());
  const domainText = (await page.locator('.brush [data-role="domain"]').innerText()).trim();
  // Same-day ranges print clock times; longer ones print dates, and the
  // separator follows the same rule.
  check(
    /^of (\d\d:\d\d:\d\d|\d\d-\d\d \d\d:\d\d)(–| → )(\d\d:\d\d:\d\d|\d\d-\d\d \d\d:\d\d)$/.test(
      domainText,
    ),
    "the brush says which range it spans",
    domainText,
  );

  const brush = await page.locator("canvas.time-brush").boundingBox();
  check(brush !== null && brush.width > 200, "the histogram brush has a size", JSON.stringify(brush));
  // The canvas must span its host, not sit at a fraction of the width. This
  // regressed when draw() pinned inline `style.width` to a stale measurement
  // and the ResizeObserver watched the canvas instead of its container.
  const brushHost = await page.locator("canvas.time-brush").evaluate((el) => {
    const host = el.parentElement;
    return host ? host.getBoundingClientRect().width : 0;
  });
  check(
    brush !== null && brushHost > 0 && brush.width >= brushHost - 2,
    "the histogram brush fills its container width",
    `canvas=${brush?.width} host=${brushHost}`,
  );
  // The window covers the whole domain, so dragging inside brackets a new one.
  await page.mouse.move(brush.x + brush.width * 0.02, brush.y + brush.height / 2);
  await page.mouse.down();
  await page.mouse.move(brush.x + brush.width * 0.35, brush.y + brush.height / 2, { steps: 6 });
  await page.mouse.up();
  await page.waitForFunction(
    () => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs > 0,
    null,
    { timeout: 10000 },
  );
  const windowState = await page.evaluate(() => ({
    from: window.lidlesseye.state.fromNs,
    until: window.lidlesseye.state.untilNs,
    domainFrom: window.lidlesseye.state.domainFrom,
    domainUntil: window.lidlesseye.state.domainUntil,
  }));
  check(
    windowState.until > windowState.from &&
      windowState.from >= windowState.domainFrom &&
      windowState.until <= windowState.domainUntil &&
      windowState.until - windowState.from < windowState.domainUntil - windowState.domainFrom,
    "the dragged window is narrower than the domain and stays inside it",
    JSON.stringify(windowState),
  );
  check((await page.url()).includes("wfrom="), "the window is in the URL alongside the domain", page.url());

  // Hovering reports the bucket under the cursor, so the brush doubles as a
  // chart: the value is the one the bar represents, not a guess.
  await page.mouse.move(brush.x + brush.width * 0.6, brush.y + brush.height / 2);
  const hoverReadout = (await page.locator('[data-testid="brush-hover"]').innerText()).trim();
  check(
    /[0-9][0-9]:[0-9][0-9]:[0-9][0-9]/.test(hoverReadout),
    "the brush reports the bucket under the cursor",
    hoverReadout,
  );

  // The brush's own zoom controls halve and double the window around its
  // centre, which is the only way to adjust it without pixel-hunting handles.
  const spanBefore = await page.evaluate(
    () => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs,
  );
  await page.click('.brush [aria-label="Zoom the window in"]');
  await page.waitForFunction(
    (span) => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs < span * 0.75,
    spanBefore,
    { timeout: 15000 },
  );
  const spanIn = await page.evaluate(
    () => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs,
  );
  check(spanIn < spanBefore * 0.75, "the brush zoom-in narrows the window", `${spanBefore} → ${spanIn}`);
  await page.click('.brush [aria-label="Zoom the window out"]');
  await page.waitForFunction(
    (span) => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs > span,
    spanIn,
    { timeout: 15000 },
  );
  const spanOut = await page.evaluate(
    () => window.lidlesseye.state.untilNs - window.lidlesseye.state.fromNs,
  );
  check(spanOut > spanIn, "the brush zoom-out widens it again", `${spanIn} → ${spanOut}`);

  await page.click('#tabs [data-view="top"]');
  await page.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });
  topText = await page.locator(".tbl-wrap table tbody").innerText();
  check(topText.includes("oldCheckout"), "the brushed window selects the older batch", topText.slice(0, 200));
  check(!topText.includes("checkoutCharge"), "the brushed window excludes the recent batch", topText.slice(0, 200));
  await shot(page, "3b-brush");

  // ---- compare ---------------------------------------------------------
  await page.click('#tabs [data-view="compare"]');
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });
  await checkActiveTab(page, "compare");
  const brushCount = await page.locator("canvas.time-brush").count();
  check(brushCount === 2, "compare shows one brush per window", String(brushCount));
  const brushLabels = (await page.locator(".brush-label").allInnerTexts()).map((text) =>
    text.toLowerCase(),
  );
  check(
    brushLabels.some((text) => text.includes("baseline")) &&
      brushLabels.some((text) => text.includes("comparison")),
    "the two brushes are labelled baseline and comparison",
    JSON.stringify(brushLabels),
  );

  // The strip above the graph names both windows and their totals; without it
  // the reader has to infer the direction of the diff from the colours.
  const summary = await page.locator('[data-testid="compare-summary"]').innerText();
  check(
    /baseline/i.test(summary) && /comparison/i.test(summary) && /→/.test(summary),
    "the compare strip names both windows",
    summary.replace(/\s+/g, " ").slice(0, 120),
  );

  // Swapping is one click, and it has to actually exchange the two windows.
  const beforeSwap = await page.evaluate(() => ({
    from: window.lidlesseye.state.fromNs,
    base: window.lidlesseye.state.baseFromNs,
  }));
  await page.click("#compare-swap");
  await page.waitForFunction(
    (before) => window.lidlesseye.state.fromNs === before.base,
    beforeSwap,
    { timeout: 15000 },
  );
  const afterSwap = await page.evaluate(() => ({
    from: window.lidlesseye.state.fromNs,
    base: window.lidlesseye.state.baseFromNs,
  }));
  check(
    afterSwap.from === beforeSwap.base && afterSwap.base === beforeSwap.from,
    "swap exchanges the baseline and comparison windows",
    JSON.stringify({ beforeSwap, afterSwap }),
  );

  // Swap back: the drag assertion below assumes the comparison bracket still
  // sits where the earlier view left it.
  await page.click("#compare-swap");
  await page.waitForFunction(
    (before) => window.lidlesseye.state.fromNs === before.from,
    beforeSwap,
    { timeout: 15000 },
  );
  // Nudge the comparison window (the second brush) to the left. Both brushes
  // share the domain, and the comparison bracket sits at 2–35% of it, so 20%
  // is inside and a small move stays within the domain.
  const baselineBrushBox = await page.locator("canvas.time-brush").nth(0).boundingBox();
  const comparisonBrushBox = await page.locator("canvas.time-brush").nth(1).boundingBox();
  check(baselineBrushBox !== null && comparisonBrushBox !== null, "both brushes have a size");
  const fromBefore = await page.evaluate(() => window.lidlesseye.state.fromNs);
  await page.mouse.move(
    comparisonBrushBox.x + comparisonBrushBox.width * 0.2,
    comparisonBrushBox.y + comparisonBrushBox.height / 2,
  );
  await page.mouse.down();
  await page.mouse.move(
    comparisonBrushBox.x + comparisonBrushBox.width * 0.17,
    comparisonBrushBox.y + comparisonBrushBox.height / 2,
    { steps: 4 },
  );
  await page.mouse.up();
  const fromAfter = await page.evaluate(() => window.lidlesseye.state.fromNs);
  check(
    fromAfter < fromBefore,
    "dragging the comparison brush moves its window independently",
    `from ${fromBefore} -> ${fromAfter}`,
  );
  await settle(page);
  await shot(page, "4-compare");

  // ---- labels ----------------------------------------------------------
  await page.click('#tabs [data-view="labels"]');
  await page.waitForSelector('.label-names button[data-name]', { timeout: 20000 });
  await checkActiveTab(page, "labels");
  const headers = await page.locator(".label-table thead th").allInnerTexts();
  const named = headers.map((text) => text.trim()).filter(Boolean);
  check(
    new Set(named).size === named.length,
    "the label table does not repeat a column header",
    JSON.stringify(named),
  );
  const names = await page.locator(".label-names button[data-name]").count();
  check(names >= 1, "label explorer lists label names", String(names));

  await page.click('.label-names button[data-name="pod"]');
  await page.waitForSelector('.label-values button[data-value]', { timeout: 20000 });
  const values = await page.locator(".label-values button[data-value]").count();
  check(values >= 1, "label explorer lists values for `pod`", String(values));

  // Capture the explorer itself: the click below navigates away.
  await shot(page, "5-labels");

  // The button's text is "Filter"; the label value lives in `data-value`.
  const firstValue = await page
    .locator(".label-values button[data-value]")
    .first()
    .getAttribute("data-value");
  await page.locator(".label-values button[data-value]").first().click();
  const filterText = (await page.locator("#filters").getAttribute("data-selector")) ?? "";
  check(
    filterText.includes("pod=") && filterText.includes(firstValue),
    "clicking a label value adds a structured filter row",
    `selector=${filterText} value=${firstValue}`,
  );
  check(
    (await page.locator(".filter-row").count()) === 1,
    "the filter bar shows exactly one row",
  );
  // Clicking a value navigates to the flamegraph, so the tab has to follow.
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });
  await checkActiveTab(page, "flamegraph");
  await settle(page);
  await shot(page, "5b-label-filtered");

  // Rows are cheap to add and remove from the bar itself.
  await page.click(".filter-add");
  check((await page.locator(".filter-row").count()) === 2, "the add button appends a row");
  await page.locator(".filter-row [data-role='remove']").nth(1).click();
  check((await page.locator(".filter-row").count()) === 1, "the remove button drops the row");
  await settle(page);

  // ---- overview --------------------------------------------------------
  await page.click('#tabs [data-view="overview"]');
  await page.waitForSelector('[data-testid="overview-card"]', { timeout: 20000 });
  await checkActiveTab(page, "overview");
  check((await page.locator('[data-testid="overview-card"]').count()) >= 3, "overview renders its cards");
  await shot(page, "6-overview");

  // ---- service map -----------------------------------------------------
  // Outside a cluster there is no traffic to draw, so the check is that the
  // view mounts and explains itself instead of rendering an empty canvas.
  await page.click('#tabs [data-view="servicemap"]');
  await checkActiveTab(page, "servicemap");
  // No cluster, so no traffic: the view must explain itself rather than show
  // an empty canvas, and still be a real mount of the new component.
  await page.waitForFunction(
    () =>
      Boolean(document.querySelector('[data-testid="service-map-canvas"]')) ||
      document.body.textContent.includes('No traffic in this range'),
    null,
    { timeout: 20000 },
  );
  check(true, "the service map view renders");
  await shot(page, "7-servicemap");
  await page.click('#tabs [data-view="overview"]');
  await page.waitForSelector('[data-testid="overview-card"]', { timeout: 20000 });

  // ---- tenant switching ------------------------------------------------
  // The label click above narrowed the selector to acme's pod; a switched
  // tenant inherits it, which is the intended behaviour but would make this
  // check assert "no rows" instead of "the other tenant's rows". Drop the
  // rows and go back to a whole-hour window first.
  const rows = await page.locator(".filter-row").count();
  for (let i = 0; i < rows; i += 1) {
    await page.locator(".filter-row [data-role='remove']").first().click();
  }
  await choose(page, "#range", "Last hour");
  await page.click("#apply");
  await page.click('#tabs [data-view="top"]');
  await page.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });

  await choose(page, "#tenant", "zeta");
  await page.waitForFunction(
    () => document.querySelector(".tbl-wrap table tbody")?.innerText.includes("zetaSearch"),
    null,
    { timeout: 20000 },
  );
  topText = await page.locator(".tbl-wrap table tbody").innerText();
  check(topText.includes("zetaSearch"), "switching tenant shows the other tenant's data", topText.slice(0, 200));
  check(!topText.includes("checkoutCharge"), "switching tenant hides the previous one", topText.slice(0, 200));
  check((await page.url()).includes("tenant=zeta"), "the tenant is reflected in the URL", page.url());
  await shot(page, "7-zeta");

  // A tenant in the query string is the form people paste into chat; it has
  // to select that tenant even when it is not the first one.
  const zetaPage = await context.newPage();
  await zetaPage.goto(`${BASE}/?tenant=zeta#/top`, { waitUntil: "load" });
  await zetaPage.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });
  check(
    (await zetaPage.locator("#tenant").innerText()).trim() === "zeta",
    "?tenant= in the query string selects that tenant",
    (await zetaPage.locator("#tenant").innerText()).trim(),
  );
  await zetaPage.close();

  // A selector fragment the row editor cannot round-trip must be kept
  // verbatim, not silently dropped: a deep link is someone's filter. The
  // server accepts unquoted values, so `service_name=checkout` is a valid
  // matcher that the editor has no dropdown representation for.
  await page.goto(
    `${BASE}/#/flamegraph?selector=pod%3D%22checkout-1%22%2Cservice_name%3Dcheckout`,
    { waitUntil: "load" },
  );
  await page.waitForSelector(".filter-row", { timeout: 20000 });
  const composed = await page.locator("#filters").getAttribute("data-selector");
  check(
    composed === 'pod="checkout-1",service_name=checkout',
    "a selector fragment that is not a matcher survives the editor",
    String(composed),
  );
  check(
    (await page.locator(".filter-row .filter-raw").count()) === 1,
    "the fragment is shown as a raw row",
  );

  // ---- an empty range must not dead-end the picker ----------------------
  // Reported symptom: choosing `pod` showed no values. Value lists were
  // scoped to the analysis window, so a window with no samples emptied them.
  // They now fall back to the last seven days, and the picker offers the
  // range that actually has data.
  const emptyNow = Date.now();
  await page.goto(`${BASE}/?tenant=zeta#/flamegraph`, { waitUntil: "load" });
  await page.waitForSelector("canvas.time-brush", { timeout: 20000 });
  await page.click('#range-mode [data-mode="absolute"]');
  await page.fill("#abs-from", localInput(new Date(emptyNow - 10 * 3_600_000)));
  await page.fill("#abs-until", localInput(new Date(emptyNow - 9 * 3_600_000)));
  await page.click("#abs-apply");
  await page.waitForFunction(() => window.lidlesseye.state.mode === "absolute", null, {
    timeout: 10000,
  });
  await page.waitForTimeout(900);
  check(
    await page.locator("#range-hint-use").isVisible(),
    "an empty range offers the range that has data",
  );
  await page.click(".filter-add");
  await choose(page, '.filter-row [data-role="label"]', "pod");
  let valueCount = 0;
  try {
    // The value picker is a searchable combobox now: the options only exist
    // while it is open.
    await page.click('.filter-row [data-role="value"]');
    await page.waitForSelector('[role="option"]', { timeout: 15000 });
    valueCount = await page.locator('[role="option"]').count();
    await page.keyboard.press("Escape");
  } catch {
    valueCount = 0;
  }
  check(
    valueCount > 0,
    "label values still load when the chosen range has no samples",
    String(valueCount),
  );

  // ---- the node panel's own flamegraph --------------------------------
  // Fresh page: the main one is parked on an empty absolute range after the
  // picker test. The seeded ClusterIP node matches `service_name=checkout`,
  // the label carried by the seeded profiles.
  const mapPage = await context.newPage();
  const mapApiCalls = [];
  mapPage.on("response", (response) => {
    if (response.url().includes("/api/")) mapApiCalls.push(response.url());
  });
  await mapPage.goto(`${BASE}/?tenant=acme#/servicemap`, { waitUntil: "load" });
  await mapPage.waitForSelector('[data-testid="map-node"]', { timeout: 20000 });
  // Profile-only controls belong to the profile queries, not to a network map.
  for (const selector of ["#profile-type", "#group-by", "#range", "#apply"]) {
    check(
      (await mapPage.locator(selector).count()) === 0,
      `the service map hides ${selector}`,
    );
  }
  check(
    await mapPage.locator("#relative-range").isVisible(),
    "relative mode offers a time range picker beside the brush",
  );
  check(
    (await mapPage.locator(".brush-label").first().innerText()).trim() === "Requests",
    "the service map histogram is labelled as request counts",
  );
  check(
    mapApiCalls.some((url) => url.includes("/api/v1/servicemap/histogram")),
    "the map asks for its own histogram",
    mapApiCalls.join(" | "),
  );
  check(
    !mapApiCalls.some((url) => url.includes("/api/v1/timeseries")),
    "the map never loads the CPU timeseries for its brush",
    mapApiCalls.join(" | "),
  );
  // The edge key names every ink the canvas draws: a grey hop, a selected hop,
  // a retransmit, and a call that leaves the cluster. Without it the four
  // styles are only learnable by hovering each one.
  const edgeLegend = await mapPage
    .locator('[data-testid="map-edge-legend"]')
    .innerText()
    .catch(() => "");
  check(
    ["call", "selected", "retransmit", "outside"].every((word) =>
      edgeLegend.toLowerCase().includes(word),
    ),
    "the map explains what each edge style means",
    edgeLegend,
  );

  const mapHistResponse = await fetch(
    `${BASE}/api/v1/servicemap/histogram?from=now-1h&buckets=60`,
    { headers: { "x-scope-orgid": "acme" } },
  );
  const mapHist = await mapHistResponse.json();
  check(
    mapHistResponse.ok &&
      Array.isArray(mapHist.points) &&
      mapHist.points.length === 60 &&
      mapHist.points.some((value) => value > 0),
    "the histogram endpoint returns per-bucket request counts",
    JSON.stringify(mapHist).slice(0, 200),
  );
  const bytesHist = await (
    await fetch(`${BASE}/api/v1/servicemap/histogram?from=now-1h&buckets=30&metric=bytes`, {
      headers: { "x-scope-orgid": "acme" },
    })
  ).json();
  check(
    bytesHist.metric === "bytes" &&
      Array.isArray(bytesHist.points) &&
      bytesHist.points.length === 30,
    "the brush can ask for a different counter",
    JSON.stringify(bytesHist).slice(0, 200),
  );
  const edgeLabelTexts = await mapPage
    .locator('[data-testid="service-map-canvas"] svg text')
    .evaluateAll((nodes) => nodes.map((node) => node.textContent ?? ""));
  // One unit for every label, so the column of latencies compares at a glance:
  // milliseconds, with the precision shrinking as the value grows.
  const latencies = edgeLabelTexts.flatMap((text) => text.match(/[\d.]+(?:ns|µs|ms|s)\b/g) ?? []);
  check(
    edgeLabelTexts.length > 0 &&
      latencies.length > 0 &&
      latencies.every((value) => value.endsWith("ms")) &&
      !latencies.some((value) => /^0ms$|^0\.0+ms$/.test(value)),
    "edge latencies are printed in milliseconds only",
    edgeLabelTexts.join(" | "),
  );

  // A workload card is titled with the workload, not with its namespace or an
  // app label; the namespace belongs on the second line.
  const cartCard = mapPage
    .locator('[data-testid="map-node"]')
    .filter({ hasText: "cart" })
    .first();
  check((await cartCard.count()) === 1, "the seeded workload is on the map");
  const cartLines = (await cartCard.innerText())
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  check(cartLines[0] === "cart", "a workload card is titled with the workload name", cartLines[0]);
  check(
    cartLines[1] === "shop · 1/1",
    "the namespace and replica count stay on the second line",
    cartLines[1],
  );

  // Clicking it opens the panel. Every workload in the one-hop drawing gets
  // container rows now — not just the node that was clicked — and each edge is
  // anchored to the container that actually opened it.
  await cartCard.click();
  await mapPage.waitForSelector('[data-testid="map-panel"][data-state="open"]', { timeout: 10000 });
  await mapPage.waitForSelector('[data-testid="map-container-row"]', { timeout: 10000 });
  const cartRows = await mapPage
    .locator('[data-testid="map-container-row"][data-node="shop/cart"]')
    .allInnerTexts();
  const paymentRows = await mapPage
    .locator('[data-testid="map-container-row"][data-node="shop/payments"]')
    .allInnerTexts();
  check(
    cartRows.length === 2 &&
      cartRows.some((row) => row.includes("istio-proxy")) &&
      cartRows.some((row) => row.includes("app")),
    "the clicked workload draws one row per container",
    JSON.stringify(cartRows),
  );
  check(
    paymentRows.length === 1 && paymentRows[0].includes("app"),
    "peer workloads draw their containers too",
    JSON.stringify(paymentRows),
  );
  // A container is a row *inside* its workload card: nested, indented, and
  // below the header. The two must never be separate boxes that can overlap.
  // Measure the card and its first row in one evaluation: the drawer widens to
  // fit the map on open, and two separate boundingBox() calls can straddle
  // that transform and read positions from different frames.
  const nesting = await mapPage.evaluate(() => {
    const card = document.querySelector('[data-testid="map-neighborhood-self"]');
    const row = document.querySelector(
      '[data-testid="map-container-row"][data-node="shop/cart"]',
    );
    if (!card || !row) return null;
    const c = card.getBoundingClientRect();
    const r = row.getBoundingClientRect();
    const zoomEl = document.querySelector('[data-testid="map-neighborhood"] > div[data-zoom]');
    return {
      c: { x: c.x, y: c.y, w: c.width, h: c.height },
      r: { x: r.x, y: r.y, h: r.height },
      zoom: Number(zoomEl?.getAttribute("data-zoom") ?? 1),
    };
  });
  check(
    nesting != null &&
      nesting.r.x > nesting.c.x + 4 &&
      nesting.r.y > nesting.c.y + 12 &&
      // The row sits just under the header. `Card`'s default py-6/gap-6 used
      // to push it down by a third of the card and clip the last row.
      nesting.r.y - nesting.c.y < 70 * nesting.zoom &&
      nesting.r.y + nesting.r.h < nesting.c.y + nesting.c.h + 1,
    "container rows are nested and indented inside their workload card",
    nesting ? `card ${JSON.stringify(nesting.c)} / row ${JSON.stringify(nesting.r)}` : "missing box",
  );
  // The whole point of the dedicated layout: no two workload cards overlap,
  // however long their names or however many containers they own.
  const cardRects = await mapPage
    .locator('[data-testid="map-neighborhood-self"],[data-testid="map-neighborhood-peer"]')
    .evaluateAll((cards) =>
      cards.map((card) => {
        const rect = card.getBoundingClientRect();
        return { x: rect.x, y: rect.y, w: rect.width, h: rect.height };
      }),
    );
  const cardOverlap = cardRects.some((a, i) =>
    cardRects.some(
      (b, j) =>
        i < j &&
        Math.min(a.x + a.w, b.x + b.w) - Math.max(a.x, b.x) > 1 &&
        Math.min(a.y + a.h, b.y + b.h) - Math.max(a.y, b.y) > 1,
    ),
  );
  check(
    cardRects.length >= 2 && !cardOverlap,
    "neighbourhood workload cards never overlap",
    JSON.stringify(cardRects),
  );
  // The arrowheads are polygons rather than SVG markers (a marker cannot
  // inherit stroke-opacity and scales with the stroke width), so the count
  // looks for the head itself.
  check(
    (await mapPage.locator('[data-testid="map-neighborhood-edges"] path[data-head]').count()) >= 2,
    "container rows carry their own edges",
  );
  // Every hop leaves a source port (`@out`) and lands on a target port
  // (`@in`); when both ends name a container (the `::` separator) the line is
  // container-to-container, not workload-to-workload.
  const hops = await mapPage
    .locator('[data-testid="map-neighborhood-edges"] path[data-from]')
    .evaluateAll((paths) =>
      paths.map((path) => ({ from: path.dataset.from, to: path.dataset.to })),
    );
  check(
    hops.length > 0 && hops.every((h) => h.from?.endsWith("@out") && h.to?.endsWith("@in")),
    "every hop is anchored on ports, not card centres",
    JSON.stringify(hops),
  );
  check(
    hops.some((h) => h.from?.includes("::") && h.to?.includes("::")),
    "at least one hop runs container-to-container",
    JSON.stringify(hops),
  );
  const ports = await mapPage
    .locator('[data-testid="map-neighborhood-self"] [data-container-port]')
    .count();
  check(ports > 0, "container rows draw connection ports", String(ports));
  // A port must sit on its own row's centre line and on the **row's** edge, not
  // on the card border: the row is the container, so that is where a hop has to
  // terminate. The port layer used to live inside the padded list, which
  // shifted every port down by the header height and left a stray pair below
  // the card.
  const portGeometry = await mapPage.evaluate(() =>
    [...document.querySelectorAll('[data-container-port]')].map((el) => {
      const portRect = el.getBoundingClientRect();
      const node = el.dataset.node ?? "";
      const container = el.dataset.container ?? "";
      const row = document.querySelector(
        `[data-testid="map-container-row"][data-node="${CSS.escape(node)}"][data-container="${CSS.escape(container)}"]`,
      );
      const card = el.closest("article");
      const rowRect = row?.getBoundingClientRect();
      const cardRect = card?.getBoundingClientRect();
      const side = el.dataset.containerPort;
      const portX = portRect.x + portRect.width / 2;
      return {
        side,
        dy: rowRect
          ? Math.abs(portRect.y + portRect.height / 2 - (rowRect.y + rowRect.height / 2))
          : null,
        // Distance from the port centre to the row edge it belongs to.
        dx: rowRect ? Math.abs(portX - (side === "out" ? rowRect.right : rowRect.left)) : null,
        // How far that edge sits inside the card's own border. Zero would mean
        // the port is still on the card, which is exactly what this guards.
        inset: rowRect && cardRect
          ? Math.round(
              side === "out" ? cardRect.right - rowRect.right : rowRect.left - cardRect.left,
            )
          : null,
      };
    }),
  );
  check(
    portGeometry.length > 0 &&
      portGeometry.every(
        (p) => p.dy != null && p.dy <= 2 && p.dx != null && p.dx <= 2.5 && (p.inset ?? 0) >= 3,
      ),
    "each port sits on its own container row's edge, inside the card",
    JSON.stringify(portGeometry),
  );
  const protocols = await mapPage
    .locator('[data-testid="map-neighborhood-edges"] path[data-protocol]')
    .evaluateAll((paths) => paths.map((path) => path.dataset.protocol));
  check(
    protocols.includes("http") && protocols.includes("grpc"),
    "edges are labelled with their application protocol",
    JSON.stringify(protocols),
  );

  // Two peers that talk to each other are drawn as a mesh curve. The star can
  // only show hops through the selected workload, so without this the cards
  // looked disconnected even when they were the busiest pair in the picture.
  const meshHops = await mapPage
    .locator('[data-testid="map-neighborhood-mesh"] path[data-from]')
    .evaluateAll((paths) => paths.map((path) => ({ from: path.dataset.from, to: path.dataset.to })));
  // Both ends lie in one column and on that column's outer edge — a caller arc
  // brackets the caller column on its left, a callee arc the callee column on
  // its right. Anything else would draw through the cards.
  const arcEnd = (anchor) => ({
    column: String(anchor).split(":")[0],
    edge: String(anchor).endsWith("@in") ? "in" : "out",
  });
  check(
    meshHops.length > 0 &&
      meshHops.every((hop) => {
        const a = arcEnd(hop.from);
        const b = arcEnd(hop.to);
        return a.column === b.column && a.edge === b.edge;
      }),
    "neighbour-to-neighbour traffic is drawn between the cards",
    JSON.stringify(meshHops.slice(0, 4)),
  );

  // Dragging the little map must not select the card labels: the second drag
  // then becomes a native text drag, which the browser turns into a search.
  // It has to pan from a card too — cards cover most of the surface. The point
  // is clamped into the window first: the drawer can be wider than the
  // viewport, and a drag aimed off-screen does nothing at all.
  // The drawer slides in, and `data-state="open"` flips before the transition
  // ends. Measuring a moving panel puts every card at the wrong place, so wait
  // for its rect to stop changing first.
  await mapPage.waitForFunction(
    () => {
      const el = document.querySelector("#map-panel");
      if (!el) return false;
      const x = Math.round(el.getBoundingClientRect().x * 10) / 10;
      const settled = window.__panelX === x;
      window.__panelX = x;
      return settled;
    },
    null,
    { timeout: 5000, polling: 100 },
  );
  const grab = await mapPage.evaluate(() => {
    const surface = document.querySelector('[data-testid="map-neighborhood"] > div[data-zoom]');
    const panel = document.querySelector("#map-panel");
    const s = surface.getBoundingClientRect();
    const p = panel.getBoundingClientRect();
    const right = Math.min(s.right, innerWidth - 6);
    const bottom = Math.min(s.bottom, innerHeight - 6);
    const cards = [
      ...document.querySelectorAll(
        '[data-testid="map-neighborhood-self"],[data-testid="map-neighborhood-peer"]',
      ),
    ];
    // A point on a card that is also inside both the surface and the window:
    // the drawer can be wider than the viewport, and a drag aimed off-screen
    // does nothing at all.
    for (const card of cards) {
      const c = card.getBoundingClientRect();
      const x = Math.min(Math.max(c.x + 24, s.x + 8), right - 40);
      const y = Math.min(Math.max(c.y + 16, s.y + 8), bottom - 30);
      const hit = document.elementFromPoint(x, y);
      if (hit?.closest("[data-map-card]")) {
        return {
          point: { x, y },
          surface: { x: s.x, y: s.y, w: s.width, h: s.height },
          panel: { x: p.x, y: p.y, w: p.width },
          viewport: { w: innerWidth, h: innerHeight },
          onCard: true,
        };
      }
    }
    return {
      point: null,
      surface: { x: s.x, y: s.y, w: s.width, h: s.height },
      panel: { x: p.x, y: p.y, w: p.width },
      viewport: { w: innerWidth, h: innerHeight },
      cards: cards.map((card) => {
        const r = card.getBoundingClientRect();
        return { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width) };
      }),
    };
  });
  check(
    grab.point !== null,
    "the drag starts on a card inside the visible part of the surface",
    JSON.stringify(grab),
  );
  // The drawer clamps its width to the window; a panel wider than the viewport
  // would hide half of the neighbourhood with no way to tell.
  check(
    grab.surface.x + grab.surface.w <= grab.viewport.w + 1 &&
      grab.panel.x >= 0,
    "the neighbourhood fits inside the window",
    JSON.stringify({ surface: grab.surface, panel: grab.panel, viewport: grab.viewport }),
  );
  const panOf = () =>
    mapPage
      .locator('[data-testid="map-neighborhood"] > div[data-zoom] > div')
      .first()
      .evaluate((el) => el.style.transform);
  const panBefore = await panOf();
  const grabX = grab.point.x;
  const grabY = grab.point.y;
  await mapPage.mouse.move(grabX, grabY);
  await mapPage.mouse.down();
  await mapPage.mouse.move(grabX + 40, grabY + 28, { steps: 6 });
  await mapPage.mouse.up();
  const selectedText = await mapPage.evaluate(() => window.getSelection()?.toString() ?? "");
  check(selectedText === "", "dragging the neighbourhood selects nothing", selectedText);
  check(
    (await panOf()) !== panBefore,
    "dragging the neighbourhood pans it",
    `${panBefore} -> ${await panOf()} from ${JSON.stringify(grab.point)}`,
  );
  // The pan must not have been read as a click on that card: the drawer still
  // shows the node it was opened on.
  check(
    (await mapPage.locator("#map-panel-title").innerText()).trim() === "cart",
    "a pan that starts on a card does not open that card",
    (await mapPage.locator("#map-panel-title").innerText()).trim(),
  );
  await mapPage.click('[data-testid="map-neighborhood-fit"]');
  const autoWidth = await mapPage
    .locator('[data-testid="map-panel"]')
    .getAttribute("data-auto-width");
  check(autoWidth === "true", "the drawer starts at the map's natural width", String(autoWidth));

  const zoomOf = () =>
    mapPage
      .locator('[data-testid="map-neighborhood"] > div[data-zoom]')
      .getAttribute("data-zoom")
      .then(Number);
  const zoomBefore = await zoomOf();
  await mapPage.click('[data-testid="map-neighborhood-zoom-in"]');
  const zoomIn = await zoomOf();
  check(zoomIn > zoomBefore, "the neighbourhood zooms in", `${zoomBefore} -> ${zoomIn}`);
  await mapPage.click('[data-testid="map-neighborhood-fit"]');
  const zoomFit = await zoomOf();
  check(
    Math.abs(zoomFit - zoomBefore) < 0.01,
    "fit returns the neighbourhood to a readable size",
    `${zoomIn} -> ${zoomFit}`,
  );

  // A peer card in the one-hop map is itself a clickable workload: pressing it
  // must walk the graph to that workload's own neighbourhood. A pan capture on
  // the card would swallow the click and leave the panel on cart.
  const peerCard = mapPage
    .locator('[data-testid="map-neighborhood-peer"]')
    .filter({ hasText: "payments" })
    .first();
  check((await peerCard.count()) === 1, "the peer workload is a card in the neighbourhood");
  await peerCard.click();
  await mapPage.waitForFunction(
    () =>
      document.querySelector("#map-panel-title")?.textContent?.trim() === "payments",
    null,
    { timeout: 5000 },
  );
  check(
    (await mapPage.locator("#map-panel-title").innerText()).trim() === "payments",
    "clicking a peer walks the graph to that workload's own neighbourhood",
  );
  // payments is only called by cart, so this hop is the mirrored drawing: a
  // CALLERS column on the left and no CALLEES column.
  await mapPage.waitForTimeout(400);
  await shot(mapPage, "9-map-callers");
  // Return to cart so the screenshot keeps the sidecar rows in view.
  const backToCart = mapPage
    .locator('[data-testid="map-neighborhood-peer"]')
    .filter({ hasText: "cart" })
    .first();
  await backToCart.click();
  await mapPage.waitForFunction(
    () => document.querySelector("#map-panel-title")?.textContent?.trim() === "cart",
    null,
    { timeout: 5000 },
  );
  await mapPage.waitForSelector('[data-testid="map-container-row"]', { timeout: 5000 });

  // The drawer slides in over 300ms; a screenshot taken mid-flight looks like
  // a clipped layout, so let the transition finish first.
  await mapPage.waitForTimeout(400);
  await shot(mapPage, "8-map-containers");
  await mapPage.click('[data-testid="map-panel-close"]');

  await mapPage
    .locator('[data-testid="map-node"]')
    .filter({ hasText: "checkout" })
    .first()
    .click();
  await mapPage.waitForSelector('[data-testid="map-panel"][data-state="open"]', { timeout: 10000 });
  await mapPage.click('[data-testid="map-tab-profiling"]');
  await mapPage.waitForSelector(
    '[data-testid="map-profile-flamegraph"] canvas.fg-canvas',
    { timeout: 20000 },
  );
  check(true, "the profiling tab embeds this node's flamegraph");

  // The drawer is portalled to the body, so it covers the whole window rather
  // than just the map card, and its left edge can be dragged to change width.
  const panelBox = await mapPage.locator('[data-testid="map-panel"]').boundingBox();
  check(
    panelBox !== null && Math.abs(panelBox.height - 900) <= 1,
    "the node panel spans the full window height",
    JSON.stringify(panelBox),
  );
  const panelWidth = () =>
    mapPage
      .locator('[data-testid="map-panel"]')
      .getAttribute("data-width")
      .then((value) => Number(value));
  const widthBefore = await panelWidth();
  const handle = mapPage.locator('[data-testid="map-panel-resize"]');
  const handleBox = await handle.boundingBox();
  await mapPage.mouse.move(
    handleBox.x + handleBox.width / 2,
    handleBox.y + handleBox.height / 2,
  );
  await mapPage.mouse.down();
  await mapPage.mouse.move(
    handleBox.x + handleBox.width / 2 - 120,
    handleBox.y + handleBox.height / 2,
    { steps: 6 },
  );
  await mapPage.mouse.up();
  const widthAfter = await panelWidth();
  check(
    widthAfter > widthBefore + 60,
    "dragging the drawer edge widens it",
    `${widthBefore} -> ${widthAfter}`,
  );

  // ---- search, focus, metric -------------------------------------------
  // A map of sixty cards has no address bar; search is it. A hit is selected,
  // centred and opens its panel, which is what "locate this node" means.
  await mapPage.click('[data-testid="map-panel-close"]');
  const totalCards = await mapPage.locator('[data-testid="map-node"]').count();
  await mapPage.fill('[data-testid="map-search"]', "cart");
  await mapPage.waitForSelector('[data-testid="map-search-hit"]', { timeout: 5000 });
  check(
    (await mapPage.locator('[data-testid="map-search-hit"]').count()) >= 1,
    "the map search lists matching nodes",
  );
  await mapPage.keyboard.press("Enter");
  await mapPage.waitForSelector('[data-testid="map-panel"][data-state="open"]', { timeout: 10000 });
  check(
    (await mapPage.locator("#map-panel-title").innerText()).trim() === "cart",
    "a search hit selects that node",
    await mapPage.locator("#map-panel-title").innerText(),
  );

  // Focus is the other half: the graph is context, the one-hop subgraph is the
  // answer, and it is laid out on its own.
  await mapPage.click('[data-testid="map-focus"]');
  await mapPage.waitForTimeout(700);
  const focusedCards = await mapPage.locator('[data-testid="map-node"]').count();
  // The seeded map is four nodes, so the one-hop drawing may legitimately be
  // all of them: the assertion is that focus reports itself and never grows
  // the drawing.
  check(
    focusedCards >= 2 && focusedCards <= totalCards,
    "focus keeps the node and its neighbours",
    `${totalCards} -> ${focusedCards}`,
  );
  check(
    (await mapPage.getAttribute('[data-testid="map-focus"]', "aria-pressed")) === "true",
    "the focus toggle reports its state",
  );
  await mapPage.click('[data-testid="map-focus"]');
  await mapPage.waitForTimeout(600);
  check(
    (await mapPage.locator('[data-testid="map-node"]').count()) === totalCards,
    "leaving focus restores the whole map",
  );
  await mapPage.click('[data-testid="map-panel-close"]');

  // The brush above the map plots one of three counters the agent records.
  await mapPage.click('[data-testid="map-metric"] button:nth-child(2)');
  await mapPage.waitForTimeout(600);
  check(
    (await mapPage.locator(".brush-label").innerText()).trim() === "Bytes",
    "the map's time chart can plot another counter",
    await mapPage.locator(".brush-label").innerText(),
  );
  await mapPage.click('[data-testid="map-metric"] button:nth-child(1)');

  // The node drawer spans the full window height and overlaps the time bar;
  // close it before reaching for the range control behind it.
  // The relative range is now a purpose-built time picker: a popover with a
  // grid of shortcuts rather than a generic Select with role=option items.
  await mapPage.click("#relative-range");
  await mapPage.locator('[data-range="15m"]').click();
  check(
    (await mapPage.evaluate(() => window.lidlesseye.state.range)) === "15m",
    "the relative range picker changes the time window",
  );
  await mapPage.close();

  // ---- language switch -------------------------------------------------
  await page.click("#settings");
  await page.waitForSelector('[data-testid="settings-dialog"]', { timeout: 10000 });
  await choose(page, "#settings-language", "Chinese");
  check(
    (await page.locator('#tabs [data-view="servicemap"] span').first().innerText()).trim() ===
      "服务地图",
    "switching the language translates the navigation",
  );
  check(
    (await page.locator("header button").filter({ hasText: "刷新" }).count()) === 1,
    "switching the language translates the header",
  );
  await choose(page, "#settings-language", "English");
  await page.keyboard.press("Escape");

  // Nothing above is worth much if the page is throwing while it renders.
  check(consoleErrors.length === 0, "no console errors", consoleErrors.join(" | "));

  await browser.close();
}

main()
  .catch(async (error) => {
    failures.push(`crashed: ${error.message}`);
    // A timeout says nothing on its own: dump what the page *did* say.
    if (page) {
      try {
        console.error("--- page text ---");
        console.error((await page.locator("body").innerText()).slice(0, 800));
        console.error("--- main html ---");
        console.error((await page.locator("#main").innerHTML()).slice(0, 800));
        const html = await page.content();
        console.error(`--- html length: ${html.length} ---`);
        console.error(html.slice(0, 700));
        if (SHOTS) await page.screenshot({ path: `${SHOTS}/crash.png`, fullPage: true });
      } catch (dumpError) {
        console.error(`could not dump the page: ${dumpError.message}`);
      }
    }
    if (consoleErrors.length) {
      console.error("--- console errors ---");
      for (const message of consoleErrors) console.error(`  ${message}`);
    }
    if (apiCalls.length) {
      console.error("--- api calls ---");
      for (const call of apiCalls) console.error(`  ${call}`);
    }
  })
  .finally(() => {
    if (failures.length) {
      console.error(`\n${failures.length} failure(s):`);
      for (const failure of failures) console.error(`  - ${failure}`);
      process.exit(1);
    }
    console.log("\nall browser checks passed");
  });
