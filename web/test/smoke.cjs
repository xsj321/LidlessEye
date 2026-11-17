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
  page.on("requestfailed", (request) =>
    consoleErrors.push(`request failed: ${request.url()} ${request.failure()?.errorText ?? ""}`),
  );
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

  // Clicking a child frame must fly to it rather than cut to it. Row 3 at a
  // quarter of the width is a wide frame in this seed no matter how the old
  // batch reorders the siblings, so the click lands inside one.
  await page.mouse.click(box.x + box.width * 0.25, box.y + ROW * 3 + ROW / 2);
  const zoomMotion = await page.evaluate(
    () => document.querySelector("canvas.fg-canvas")?.dataset.motion ?? null,
  );
  check(zoomMotion === "zoom", "clicking a frame starts a viewport transition", String(zoomMotion));
  await settle(page);
  const zoomNote = await page.locator('.fg-legend [data-role="zoom"]').innerText().catch(() => "");
  check(/% of total shown/.test(zoomNote), "the zoom landed on the clicked frame", zoomNote);

  await page.keyboard.press("Escape");
  await settle(page);
  check(
    (await page.locator('.fg-legend [data-role="zoom"]').count()) === 0,
    "Escape animates back to the whole graph",
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
  check(/^of \d\d:\d\d:\d\d–\d\d:\d\d:\d\d$/.test(domainText), "the brush says which range it spans", domainText);

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
  const mapHistResponse = await fetch(
    `${BASE}/api/v1/servicemap/histogram?from=now-1h&buckets=60`,
    { headers: { "x-scope-orgid": "acme" } },
  );
  const mapHist = await mapHistResponse.json();
  check(
    mapHistResponse.ok &&
      Array.isArray(mapHist.connections) &&
      mapHist.connections.length === 60 &&
      mapHist.connections.some((value) => value > 0),
    "the histogram endpoint returns per-bucket request counts",
    JSON.stringify(mapHist).slice(0, 200),
  );
  const edgeLabelTexts = await mapPage
    .locator('[data-testid="service-map-canvas"] svg text')
    .evaluateAll((nodes) => nodes.map((node) => node.textContent ?? ""));
  check(
    edgeLabelTexts.length > 0 &&
      edgeLabelTexts.some((text) => text.includes("ms")) &&
      !edgeLabelTexts.some((text) => /1000(ms|µs|s)|0\.00\d*\s*(s|ms|µs)/.test(text)),
    "edge latencies pick a unit with a digit in the ones place",
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
  // A container row must sit clearly below its workload card, not glued to it:
  // the two are different things and touching text reads as one cramped block.
  const selfBox = await mapPage.locator('[data-testid="map-neighborhood-self"]').boundingBox();
  const firstRowBox = await mapPage
    .locator('[data-testid="map-container-row"][data-node="shop/cart"]')
    .first()
    .boundingBox();
  check(
    selfBox != null &&
      firstRowBox != null &&
      firstRowBox.y - (selfBox.y + selfBox.height) >= 4,
    "container rows keep a visible gap below their workload card",
    selfBox && firstRowBox
      ? `card bottom ${(selfBox.y + selfBox.height).toFixed(1)} / row top ${firstRowBox.y.toFixed(1)}`
      : "missing box",
  );
  check(
    (await mapPage.locator('[data-testid="map-neighborhood-edges"] path[marker-end]').count()) >= 2,
    "container rows carry their own edges",
  );
  const protocols = await mapPage
    .locator('[data-testid="map-neighborhood-edges"] path[data-protocol]')
    .evaluateAll((paths) => paths.map((path) => path.dataset.protocol));
  check(
    protocols.includes("http") && protocols.includes("grpc"),
    "edges are labelled with their application protocol",
    JSON.stringify(protocols),
  );
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

  // The node drawer spans the full window height and overlaps the time bar;
  // close it before reaching for the range control behind it.
  await mapPage.click('[data-testid="map-panel-close"]');
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
