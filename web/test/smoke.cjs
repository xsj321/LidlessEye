// Browser smoke test for the embedded dashboard.
//
// The Rust suites cover the API; this covers what a human actually clicks:
// every tab renders, the flamegraph paints real pixels, the label explorer
// writes back into the selector, and switching tenant really switches data.
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
function report({ service, pod, stacks, nowNs }) {
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
    start_ns: nowNs - 60_000_000_000,
    end_ns: nowNs,
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
  await ingest("acme", [
    report({
      service: "checkout",
      pod: "checkout-1",
      nowNs,
      stacks: [
        [["checkoutCharge", "handleHTTP", "main"], 500_000_000],
        [["checkoutValidate", "handleHTTP", "main"], 200_000_000],
        [["pgQuery", "checkoutCharge", "handleHTTP", "main"], 120_000_000],
      ],
    }),
  ]);
  await ingest("zeta", [
    report({
      service: "search",
      pod: "search-1",
      nowNs,
      stacks: [
        [["zetaSearch", "handleHTTP", "main"], 300_000_000],
        [["zetaRank", "zetaSearch", "handleHTTP", "main"], 90_000_000],
      ],
    }),
  ]);
}

/** The highlighted tab must name the view that is actually rendered. */
async function checkActiveTab(page, expected) {
  const active = await page.locator("#tabs a.active").evaluateAll((links) =>
    links.map((link) => link.dataset.view),
  );
  check(
    active.length === 1 && active[0] === expected,
    `the ${expected} tab is the only one highlighted`,
    JSON.stringify(active),
  );
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

  // ---- flamegraph ------------------------------------------------------
  const navigation = await page.goto(`${BASE}/#/flamegraph`, { waitUntil: "load" });
  check(navigation !== null && navigation.status() === 200, "the dashboard responds with 200", String(navigation && navigation.status()));
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });

  check((await page.title()) === "LidlessEye", "page title");
  check((await page.locator("#tabs a").count()) === 6, "six tabs are rendered");

  const box = await page.locator("canvas.fg-canvas").boundingBox();
  check(box !== null && box.width > 100 && box.height > 100, "flamegraph canvas has a size", JSON.stringify(box));
  await page.waitForTimeout(300);
  await shot(page, "1-flamegraph");

  // The tenant selector is only shown when more than one tenant exists.
  const initialTenant = await page.locator("#tenant").inputValue();
  check(initialTenant === "acme", "the tenant selector defaults to the first tenant", initialTenant);
  check(!(await page.locator("#tenant").isHidden()), "tenant selector is visible with two tenants");

  // ---- top functions ---------------------------------------------------
  await page.click('#tabs a[data-view="top"]');
  await page.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });
  await checkActiveTab(page, "top");
  let topText = await page.locator(".tbl-wrap table tbody").innerText();
  check(topText.includes("checkoutCharge"), "top table lists acme's functions", topText.slice(0, 200));
  check(!topText.includes("zetaSearch"), "top table does not leak another tenant", topText.slice(0, 200));
  await shot(page, "2-top");

  // ---- time series -----------------------------------------------------
  await page.click('#tabs a[data-view="timeseries"]');
  await page.waitForSelector("main canvas", { timeout: 20000 });
  await checkActiveTab(page, "timeseries");
  check((await page.locator("main canvas").count()) >= 1, "time series renders a chart");
  await shot(page, "3-timeseries");

  // ---- compare ---------------------------------------------------------
  await page.click('#tabs a[data-view="compare"]');
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });
  await checkActiveTab(page, "compare");
  check(!(await page.locator("#baseline-field").isHidden()), "compare reveals the baseline control");
  await shot(page, "4-compare");

  // ---- labels ----------------------------------------------------------
  await page.click('#tabs a[data-view="labels"]');
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

  const firstValue = await page.locator(".label-values button[data-value]").first().innerText();
  await page.locator(".label-values button[data-value]").first().click();
  const selector = await page.locator("#selector").inputValue();
  check(
    selector.includes("pod="),
    "clicking a label value writes it into the selector",
    `selector=${selector} value=${firstValue}`,
  );
  // Clicking a value navigates to the flamegraph, so the tab has to follow.
  await page.waitForSelector("canvas.fg-canvas", { timeout: 20000 });
  await checkActiveTab(page, "flamegraph");
  await shot(page, "5b-label-filtered");

  // ---- overview --------------------------------------------------------
  await page.click('#tabs a[data-view="overview"]');
  await page.waitForSelector(".cards .card", { timeout: 20000 });
  await checkActiveTab(page, "overview");
  check((await page.locator(".cards .card").count()) >= 3, "overview renders its cards");
  await shot(page, "6-overview");

  // ---- tenant switching ------------------------------------------------
  // The label click above narrowed the selector to acme's pod; a switched
  // tenant inherits it, which is the intended behaviour but would make this
  // check assert "no rows" instead of "the other tenant's rows".
  await page.fill("#selector", "");
  await page.click("#apply");
  await page.click('#tabs a[data-view="top"]');
  await page.waitForSelector(".tbl-wrap table tbody tr", { timeout: 20000 });

  await page.selectOption("#tenant", "zeta");
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
