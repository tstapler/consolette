// Headless smoke for the Overview page: the first trace row shows real cells and Inspect opens the drawer.
//
// Usage (against an already running proxy with one working upstream; this script only
// POSTs one /v1/messages request and loads pages, it starts and stops nothing):
//   SMOKE_BASE=http://127.0.0.1:19100 npm run e2e
// Env: SMOKE_BASE (default http://127.0.0.1:8080), SMOKE_MODEL (default claude-sonnet-4),
//      PUPPETEER_EXECUTABLE_PATH or CHROME_PATH (system Chrome; default is puppeteer's bundled one).
import puppeteer from 'puppeteer';

const base = (process.env.SMOKE_BASE ?? 'http://127.0.0.1:8080').replace(/\/$/, '');
const model = process.env.SMOKE_MODEL ?? 'claude-sonnet-4';
const executablePath = process.env.PUPPETEER_EXECUTABLE_PATH ?? process.env.CHROME_PATH;
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`);
  if (!ok) failures.push(what);
};

async function sendOneRequest() {
  const nonce = `smoke-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  const res = await fetch(`${base}/v1/messages`, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-api-key': 'smoke', 'anthropic-version': '2023-06-01' },
    body: JSON.stringify({ model, max_tokens: 16, messages: [{ role: 'user', content: `overview smoke ${nonce}` }] }),
  });
  await res.arrayBuffer();
  return res.status;
}

const browser = await puppeteer.launch({ headless: true, args: ['--no-sandbox'], ...(executablePath ? { executablePath } : {}) });
try {
  const page = await browser.newPage();
  await page.setViewport({ width: 1440, height: 900 });
  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(e.message));

  // Load first so the live stream is connected, then send: the row arrives over SSE.
  await page.goto(`${base}/dashboard/overview`, { waitUntil: 'networkidle2' });
  const status = await sendOneRequest();
  check(status === 200, `proxy answered the smoke request with 200 (got ${status})`);

  await page.waitForSelector('tr[data-row]', { timeout: 15000 });
  const rowText = await page.$eval('tr[data-row]', (r) => r.innerText);
  console.log(`first row: ${rowText.replace(/\s+/g, ' ').trim()}`);
  check(/\b200 OK\b/.test(rowText), 'first row matches /\\b200 OK\\b/');
  check(!/undefined|NaN|\[object/.test(rowText), 'first row has no undefined/NaN/[object]');

  await page.click('tr[data-row] button[data-inspect]');
  await page.waitForSelector('[role="dialog"] [data-requested]', { timeout: 5000 });
  const requested = await page.$eval('[role="dialog"] [data-requested]', (e) => e.innerText);
  check(/Requested/.test(requested), `Inspect drawer shows "Requested" (${requested.trim()})`);
  check(pageErrors.length === 0, `no uncaught page errors (${pageErrors.length})`);
} catch (e) {
  failures.push(String(e));
  console.log(`FAIL ${e}`);
} finally {
  await browser.close();
}
console.log(failures.length === 0 ? 'SMOKE OK' : `SMOKE FAILED (${failures.length})`);
process.exit(failures.length === 0 ? 0 : 1);
