import { test, before, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { launchBrowser, openConsole, startStack } from './browser-fixture.mjs';

let stack;
let browser;
let session;

before(async () => {
  stack = await startStack();
  browser = await launchBrowser();
  session = await openConsole(stack, browser);
});

after(async () => {
  await session?.context.close();
  await browser?.close();
  await stack?.close();
});

test('real browser loads and renders the operator console', async () => {
  const { page } = session;
  await page.getByRole('heading', { name: 'DB Gateway' }).waitFor();
  await page.getByText('Gateway', { exact: true }).waitFor();
  assert.equal(await page.locator('#summary article').count() > 0, true);
  assert.equal(await page.locator('#signals article').count(), 7);
  assert.match(await page.locator('#tunnel-status').innerText(), /not_configured/);
  assert.deepEqual(session.browserErrors, []);
});

test('real browser opens a data-centric database detail view', async () => {
  const { page } = session;
  const response = await page.request.post(`${stack.gatewayBase}/v1/databases`, {
    headers: { Authorization: `Bearer ${stack.ownerKey}` },
    data: { name: 'browserdb' },
  });
  assert.equal(response.ok(), true, await response.text());
  await page.reload({ waitUntil: 'networkidle' });
  await page.getByRole('button', { name: 'browserdb', exact: true }).click();
  await page.locator('#database-detail').waitFor();
  await page.getByText('Table statistics', { exact: true }).waitFor();
  assert.match(await page.locator('#database-detail').innerText(), /browserdb/);
  assert.match(await page.locator('#database-detail').innerText(), /No user tables reported/);
});

test('real browser query, batch, and lifecycle controls use the companion', async () => {
  const { page, requests } = session;
  page.once('dialog', (dialog) => dialog.accept());
  await page.locator('#database').selectOption('browserdb');
  await page.locator('#sql').fill('CREATE TABLE items (id INTEGER PRIMARY KEY, label TEXT)');
  await page.getByRole('button', { name: 'Run query' }).click();
  await page.locator('#result').getByText('Rows affected: 0', { exact: true }).waitFor();
  await page.locator('#batch-database').selectOption('browserdb');
  await page.locator('#batch-json').fill('{"atomic":true,"statements":[{"sql":"SELECT count(*) FROM items","args":[]}]}');
  await page.getByRole('button', { name: 'Run atomic batch' }).click();
  await page.locator('#batch-result').getByText('Rows affected: 0', { exact: true }).waitFor();
  await page.locator('#clone-source').selectOption('browserdb');
  await page.locator('#clone-destination').fill('browserclone');
  await page.getByRole('button', { name: 'Clone database' }).click();
  await page.locator('#lifecycle-result').getByText(/browserclone/, { exact: false }).waitFor();
  assert.equal(requests.some((request) => request.url.endsWith('/api/databases/browserdb/query') && request.method === 'POST'), true);
  assert.equal(requests.some((request) => request.url.endsWith('/api/databases/browserdb/clone') && request.method === 'POST'), true);
});

test('real browser exercises keys, verified transfer controls, and tunnel lifecycle', async () => {
  const { page } = session;
  await page.locator('#key-name').fill('browserkey');
  await page.locator('#key-db').fill('browserdb');
  await page.getByRole('button', { name: 'Issue one-time key' }).click();
  await page.locator('#key-result').getByText(/"key"/, { exact: false }).waitFor();
  page.once('dialog', (dialog) => dialog.accept());
  await page.locator('[data-revoke="browserkey"]').click();
  await page.locator('[data-revoke="browserkey"]').waitFor({ state: 'detached' });

  await page.locator('#export-db').selectOption('browserdb');
  await page.getByRole('button', { name: 'Export verified SQLite' }).click();
  await page.locator('#export-result').getByText(/complete/, { exact: false }).waitFor();
  const downloadLink = page.locator('#export-result a');
  assert.equal(await downloadLink.getAttribute('download'), 'browserdb.sqlite');

  const exportedResponse = await page.request.post(`${stack.gatewayBase}/v1/databases/browserdb/export`, {
    headers: { Authorization: `Bearer ${stack.ownerKey}` },
    data: {},
  });
  assert.equal(exportedResponse.ok(), true, await exportedResponse.text());
  const exported = await exportedResponse.json();
  const directory = await mkdtemp(join(tmpdir(), 'dg-browser-import-'));
  const file = join(directory, 'browserdb.sqlite');
  await writeFile(file, Buffer.from(exported.data_base64, 'base64'));
  await page.locator('#import-file').setInputFiles(file);
  await page.locator('#import-name').fill('browserimported');
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: 'Import verified SQLite' }).click();
  await page.locator('#import-result').getByText(/browserimported/, { exact: false }).waitFor();
  await rm(directory, { recursive: true, force: true });

  await page.getByRole('button', { name: 'Start tunnel' }).click();
  await page.locator('#tunnel-status').getByText(/not_configured/, { exact: false }).waitFor();
  await page.getByRole('button', { name: 'Stop tunnel' }).click();
});

test('real browser shows loading and disabled state during duplicate submissions', async () => {
  const { page } = session;
  await page.route('**/api/databases/browserdb/query', async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 150));
    await route.continue();
  });
  const form = page.locator('#query-form');
  const button = form.getByRole('button', { name: 'Run query' });
  await page.locator('#database').selectOption('browserdb');
  await page.locator('#sql').fill('SELECT 1');
  await button.click();
  assert.equal(await button.isDisabled(), true);
  assert.equal(await form.getAttribute('aria-busy'), 'true');
  await page.waitForTimeout(250);
  assert.equal(await button.isDisabled(), false);
  assert.equal(await form.getAttribute('aria-busy'), 'false');
  await page.unroute('**/api/databases/browserdb/query');
});

test('real browser guards destructive and publish actions', async () => {
  const { page } = session;
  let confirmations = 0;
  page.on('dialog', async (dialog) => { confirmations += 1; await dialog.dismiss(); });
  await page.locator('#delete-db').fill('browserdb');
  await page.locator('#confirm-db').fill('different_name');
  await page.getByRole('button', { name: 'Delete with confirmation' }).click();
  assert.equal(await page.locator('#result-actions').innerText().then((text) => /exact database name/i.test(text)), true);
  await page.locator('#confirm-db').fill('browserdb');
  await page.getByRole('button', { name: 'Delete with confirmation' }).click();
  assert.equal(await page.locator('#lifecycle-result').innerText().then((text) => /cancelled/i.test(text)), true);
  await page.locator('#clone-source').selectOption('browserdb');
  await page.locator('#clone-destination').fill('browserclone_guard');
  await page.getByRole('button', { name: 'Clone database' }).click();
  assert.equal(await page.locator('#lifecycle-result').innerText().then((text) => /cancelled/i.test(text)), true);
  assert.equal(confirmations >= 2, true);
});

test('real browser remains responsive and exposes accessible labels at narrow width', async () => {
  const { page } = session;
  await page.setViewportSize({ width: 320, height: 800 });
  const metrics = await page.evaluate(() => ({
    innerWidth: window.innerWidth,
    scrollWidth: document.documentElement.scrollWidth,
    unlabeled: [...document.querySelectorAll('input, select, textarea')].filter((control) => {
      if (control.getAttribute('aria-label')) return false;
      return !control.id || !document.querySelector(`label[for="${control.id}"]`);
    }).length,
    busyMain: document.querySelector('#console-main')?.getAttribute('aria-busy'),
  }));
  assert.equal(metrics.scrollWidth <= metrics.innerWidth, true, JSON.stringify(metrics));
  assert.equal(metrics.unlabeled, 0, JSON.stringify(metrics));
  assert.equal(metrics.busyMain, 'false');
});

test('real browser never receives or stores the owner credential', async () => {
  const { page, context } = session;
  const html = await page.content();
  const storage = await page.evaluate(() => ({
    local: { ...localStorage }, session: { ...sessionStorage }, cookie: document.cookie,
  }));
  const browserText = JSON.stringify({ html, storage });
  assert.equal(browserText.includes(stack.ownerKey), false);
  assert.equal(await context.cookies().then((cookies) => cookies.some((cookie) => cookie.value.includes(stack.ownerKey))), false);
});
