const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]));
const MAX_TRANSFER_BYTES = 128 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES = 1024 * 1024;
const GRID_PAGE_SIZE = 50;
let selectedDatabase = null;
let detailTab = 'overview';
let databaseNames = [];
let enrichedDatabases = [];
let overviewCache = null;
let detailCache = null;
let exportTransfer = null;
let exportBlobUrl = null;
let importTransfer = null;
let gridState = { table: null, offset: 0, hasMore: false };
let gridUnordered = new Set();
let gridSchemaKnown = true;
let detailGeneration = 0;
const pending = new WeakMap();
// Console input history is deliberately in-memory only: statements can
// carry sensitive literals, so they must never reach persistent browser
// storage or cookies (pinned by web-console-security-check).
const QUERY_HISTORY_LIMIT = 50;
const consoleHistories = new Map();
function historyFor(textareaId) {
  let history = consoleHistories.get(textareaId);
  if (!history) { history = { entries: [], index: 0, draft: '' }; consoleHistories.set(textareaId, history); }
  return history;
}
function pushConsoleHistory(textareaId, statement) {
  const trimmed = String(statement ?? '').trim();
  if (!trimmed) return;
  const history = historyFor(textareaId);
  if (history.entries[history.entries.length - 1] !== trimmed) {
    history.entries.push(trimmed);
    if (history.entries.length > QUERY_HISTORY_LIMIT) history.entries.shift();
  }
  history.index = history.entries.length;
  history.draft = '';
}
function stepConsoleHistory(textareaId, direction) {
  const area = $(textareaId);
  const history = historyFor(textareaId);
  if (!area || !history.entries.length) return false;
  if (history.index === history.entries.length) history.draft = area.value;
  const next = history.index + direction;
  if (next < 0 || next > history.entries.length) return false;
  history.index = next;
  area.value = next === history.entries.length ? history.draft : history.entries[next];
  area.selectionStart = area.selectionEnd = area.value.length;
  return true;
}
// Ctrl+Enter (Cmd+Enter on macOS) submits; Up/Down recalls history only
// when the caret sits on the first/last visual line, so multi-line
// editing keeps its normal cursor behavior.
function wireConsoleInput(textareaId, formId) {
  const area = $(textareaId);
  if (!area) return;
  area.addEventListener('keydown', (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
      event.preventDefault();
      $(formId)?.requestSubmit();
      return;
    }
    if (event.key !== 'ArrowUp' && event.key !== 'ArrowDown') return;
    if (area.selectionStart !== area.selectionEnd) return;
    const before = area.value.slice(0, area.selectionStart);
    const after = area.value.slice(area.selectionEnd);
    if (event.key === 'ArrowUp' && before.includes('\n')) return;
    if (event.key === 'ArrowDown' && after.includes('\n')) return;
    if (stepConsoleHistory(textareaId, event.key === 'ArrowUp' ? -1 : 1)) event.preventDefault();
  });
}

async function request(path, { method = 'GET', body, headers = {} } = {}) {
  const init = { method, headers: { ...headers }, redirect: 'error' };
  if (body !== undefined) {
    init.headers['content-type'] = 'application/json';
    init.body = JSON.stringify(body);
  }
  const response = await fetch(path, init);
  const text = await response.text();
  let value;
  try { value = text ? JSON.parse(text) : null; } catch { value = { error: { code: 'invalid_response', message: 'The console returned invalid JSON.' } }; }
  if (!response.ok) {
    const error = new Error(value?.error?.message || 'Request failed');
    error.code = value?.error?.code || 'http_error';
    error.status = response.status;
    throw error;
  }
  return value;
}
const get = (path) => request(path);
const post = (path, body, headers = {}) => request(path, { method: 'POST', body, headers });

function setFormBusy(form, busy, message = '') {
  if (!form) return;
  form.setAttribute('aria-busy', String(busy));
  form.querySelectorAll('button, input, select, textarea').forEach((control) => { control.disabled = busy; });
  const status = form.querySelector('.form-status');
  if (status) status.textContent = message || (busy ? 'Working…' : '');
}
async function runForm(form, message, work) {
  if (pending.get(form)) return;
  pending.set(form, true);
  setFormBusy(form, true, message);
  let failed = null;
  try { await work(); }
  catch (error) { failed = error; }
  pending.set(form, false);
  if (failed) {
    // Errors stay on the submitting form's own status line, which is on
    // the same tab the user is looking at — never a hidden panel.
    form.setAttribute('aria-busy', 'false');
    form.querySelectorAll('button, input, select, textarea').forEach((control) => { control.disabled = false; });
    const status = form.querySelector('.form-status');
    if (status) status.textContent = failed.message || 'Request failed';
  } else setFormBusy(form, false);
}
function showError(target, error) {
  const element = $(target);
  if (element) element.textContent = JSON.stringify({ error: { code: error.code || 'console_error', status: error.status, message: error.message } }, null, 2);
}
function setText(target, value) { const element = $(target); if (element) element.textContent = value; }
function fmtBytes(bytes) {
  if (!Number.isFinite(bytes)) return 'unavailable';
  if (bytes < 1024) return `${bytes} B`;
  const units = ['KB', 'MB', 'GB'];
  let value = bytes / 1024;
  let unit = units[0];
  for (const next of units) { unit = next; if (value < 1024 || next === 'GB') break; value /= 1024; }
  return `${value.toFixed(value < 10 ? 2 : 1)} ${unit}`;
}
function renderTable(target, columns, rows, rowsAffected) {
  const element = $(target);
  if (!element) return;
  const affected = rowsAffected == null ? '' : `<p class="muted">Rows affected: ${esc(rowsAffected)}</p>`;
  if (!columns.length) { element.innerHTML = `${affected}<p class="muted">No columns returned.</p>`; return; }
  element.innerHTML = `${affected}<div class="table-wrap"><table><thead><tr>${columns.map((column) => `<th scope="col">${esc(column)}</th>`).join('')}</tr></thead><tbody>${rows.map((row) => `<tr>${columns.map((_, index) => `<td>${esc(row[index])}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}
function renderBatchResults(target, results) {
  const element = $(target);
  if (!element) return;
  const sets = Array.isArray(results) ? results : [];
  if (!sets.length) { renderTable(target, [], [], null); return; }
  element.innerHTML = sets.map((result, index) => {
    const columns = result?.columns ?? [];
    const rows = result?.rows ?? [];
    const affected = result?.rows_affected == null ? '' : `<p class="muted">Rows affected: ${esc(result.rows_affected)}</p>`;
    const body = !columns.length ? '<p class="muted">No columns returned.</p>'
      : `<div class="table-wrap"><table><thead><tr>${columns.map((column) => `<th scope="col">${esc(column)}</th>`).join('')}</tr></thead><tbody>${rows.map((row) => `<tr>${columns.map((_, cell) => `<td>${esc(row[cell])}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
    return `<section><h4>Statement ${index + 1} of ${sets.length}</h4>${affected}${body}</section>`;
  }).join('');
}
function parseHash() {
  try {
    const parts = (window.location.hash || '#/databases').replace(/^#\//, '').split('/').map(decodeURIComponent);
    if (parts[0] === 'status') return { view: 'status' };
    if (parts[0] === 'databases' && parts[1]) {
      const tab = parts[2] === 'data' || parts[2] === 'tokens' ? parts[2] : 'overview';
      return { view: 'detail', name: parts[1], tab };
    }
    return { view: 'databases' };
  } catch (error) {
    // A stray % in a pasted/shared URL (or a browser extension rewriting the
    // hash) produces URIError from decodeURIComponent; fall back to the
    // default view rather than crashing the page on a hashchange.
    return { view: 'databases' };
  }
}
function navigate(view, name = null, tab = 'overview') {
  if (view === 'status') window.location.hash = '#/status';
  else if (view === 'detail' && name) window.location.hash = `#/databases/${encodeURIComponent(name)}${tab === 'overview' ? '' : `/${tab}`}`;
  else window.location.hash = '#/databases';
}
function showView(route) {
  document.querySelectorAll('[data-view-panel]').forEach((panel) => { panel.hidden = panel.dataset.viewPanel !== route.view; });
  document.querySelectorAll('nav [data-view]').forEach((button) => {
    const active = button.dataset.view === route.view || (route.view === 'detail' && button.dataset.view === 'databases');
    if (active) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
  });
  if (route.view === 'detail') {
    document.querySelectorAll('[role="tab"]').forEach((tab) => tab.setAttribute('aria-selected', String(tab.dataset.tab === route.tab)));
    document.querySelectorAll('[data-tab-panel]').forEach((panel) => { panel.hidden = panel.dataset.tabPanel !== route.tab; });
  }
}
function renderBackups(backups) {
  const entries = Object.entries(backups?.per_database ?? {});
  // `unavailable` is the overview's degraded marker: an empty per_database
  // without it reads as a clean backup state, which an unreachable backup
  // root must never look like.
  const empty = backups?.unavailable ? '<p class="state state-error">Backup freshness unavailable.</p>' : '<p class="muted">No database freshness records reported.</p>';
  $('backups-summary').innerHTML = entries.length ? entries.map(([name, value]) => `<article><b>${esc(name)}</b><strong>${esc(value.state)}</strong><small>${value.age_secs == null ? 'Age unavailable' : `${value.age_secs}s old`}</small></article>`).join('') : empty;
  $('backups').textContent = JSON.stringify(backups ?? { state: 'unknown' }, null, 2);
}
function renderDbTable() {
  const cell = (value) => (value === null || value === undefined ? '<span class="muted">unavailable</span>' : esc(value));
  const rows = enrichedDatabases.map((db) => `<tr><td><button type="button" class="db" data-name="${esc(db.name)}">${esc(db.name)}</button></td><td>Database</td><td>${cell(db.rows_read)}</td><td>${cell(db.rows_written)}</td><td>${db.size_bytes === null || db.size_bytes === undefined ? '<span class="muted">unavailable</span>' : esc(fmtBytes(db.size_bytes))}</td><td>${esc(db.backup?.state ?? 'unknown')}</td><td><span class="status-pill">${db.unknown ? 'Unknown' : db.tombstoned ? 'tombstoned' : 'Active'}</span></td><td><button type="button" data-data="${esc(db.name)}">Data</button></td></tr>`).join('');
  $('db-rows').innerHTML = rows || `<tr><td colspan="8" class="muted">No databases reported.</td></tr>`;
  document.querySelectorAll('.db').forEach((button) => button.addEventListener('click', () => navigate('detail', button.dataset.name, 'overview')));
  document.querySelectorAll('[data-data]').forEach((button) => button.addEventListener('click', () => navigate('detail', button.dataset.data, 'data')));
}
function renderDetail(detail, tab) {
  detailCache = detail;
  const state = $('detail-state');
  const content = $('detail-content');
  state.hidden = true; state.className = 'state state-empty';
  content.hidden = false;
  $('detail-name').textContent = detail.name;
  $('detail-crumb').textContent = detail.name;
  $('detail-pill').textContent = detail.tombstoned ? 'tombstoned' : 'Active';
  const tables = Array.isArray(detail.tables) ? detail.tables : [];
  const schemaRows = detail.schema?.rows ?? [];
  const entry = enrichedDatabases.find((db) => db.name === detail.name);
  const usageOr = (value) => (value === null || value === undefined ? 'unavailable' : value);
  $('detail-activity').innerHTML = [['Rows read', usageOr(entry?.rows_read)], ['Rows written', usageOr(entry?.rows_written)], ['Storage', fmtBytes(detail.size_bytes)], ['Tables', tables.length]].map(([key, value]) => `<article><b>${esc(key)}</b><strong>${esc(value)}</strong></article>`).join('');
  $('connect-url').textContent = overviewCache?.server_url ? `${overviewCache.server_url}  (database: ${detail.name})` : 'Server URL unavailable.';
  $('detail-tables').innerHTML = `<p class="muted">${detail.size_bytes} bytes · created ${detail.created_at ?? 'unknown'}</p>${tables.length ? `<div class="table-wrap"><table><thead><tr><th>Table</th><th>Rows</th></tr></thead><tbody>${tables.map((table) => `<tr><td>${esc(table.name)}</td><td>${esc(table.rows)}</td></tr>`).join('')}</tbody></table></div>` : '<p class="muted">No user tables reported.</p>'}`;
  $('detail-schema').innerHTML = schemaRows.length ? schemaRows.map((row) => `<details><summary>${esc(row[0])}</summary><pre>${esc(row[1] ?? 'Schema unavailable')}</pre></details>`).join('') : '<p class="muted">No schema statements reported.</p>';
  const backup = entry?.backup ?? overviewCache?.backups?.per_database?.[detail.name] ?? { state: 'unknown' };
  $('detail-backup').innerHTML = `<article><b>Freshness</b><strong>${esc(backup.state)}</strong><small>${backup.age_secs == null ? 'Age unavailable' : `${backup.age_secs}s old`}</small></article>`;
  const keys = (Array.isArray(overviewCache?.keys) ? overviewCache.keys : []).filter((key) => (key.databases || []).includes(detail.name));
  $('key-list').innerHTML = keys.length ? keys.map((key) => `<article><b>${esc(key.name)}</b><strong>${esc(key.role)}</strong><small>${esc((key.databases || []).join(', '))}</small>${key.name === 'owner' ? '' : `<button data-revoke="${esc(key.name)}">Revoke</button>`}</article>`).join('') : '<p class="muted">No scoped keys for this database.</p>';
  document.querySelectorAll('[data-revoke]').forEach((button) => button.addEventListener('click', async () => { if (!window.confirm(`Revoke key ${button.dataset.revoke}?`)) return; try { await request(`/api/keys/${encodeURIComponent(button.dataset.revoke)}`, { method: 'DELETE' }); await refresh(); } catch (error) { showError('key-result', error); } }));
  for (const id of ['database', 'batch-database', 'clone-source', 'export-db']) { setOptions(id, databaseNames); $(id).value = detail.name; }
  $('key-db').value = detail.name;
  $('delete-db').value = detail.name;
  renderGridTables(tables);
  // When the schema join failed the safeguard set is empty *and* unknown:
  // assuming rowid then emits ORDER BY rowid against a table that may be
  // WITHOUT ROWID. Degrade to unordered paging instead of erroring.
  gridSchemaKnown = detail.schema != null;
  gridUnordered = new Set(schemaRows.filter((row) => /WITHOUT\s+ROWID/i.test(row[1] ?? '')).map((row) => row[0]));
  if (tab === 'data' && gridState.table) void loadGridPage();
}
function renderGridTables(tables) {
  const select = $('grid-table');
  const names = tables.map((table) => table.name);
  setOptions('grid-table', names);
  if (!names.includes(gridState.table)) { gridState = { table: names[0] ?? null, offset: 0, hasMore: false }; }
  select.value = gridState.table ?? '';
  if (!gridState.table) { $('grid-result').innerHTML = '<p class="muted">No tables to browse.</p>'; $('grid-page').textContent = ''; }
}
async function loadGridPage() {
  const { table, offset } = gridState;
  if (!table || !selectedDatabase) return;
  $('grid-page').textContent = 'Loading…';
  try {
    const quoted = `"${table.replace(/"/g, '""')}"`;
    const order = gridUnordered.has(table) || !gridSchemaKnown ? '' : ' ORDER BY rowid';
    const value = await post(`/api/databases/${encodeURIComponent(selectedDatabase)}/query`, { sql: `SELECT * FROM ${quoted}${order} LIMIT ${GRID_PAGE_SIZE + 1} OFFSET ${offset}`, args: [] });
    const rows = value.rows ?? [];
    gridState.hasMore = rows.length > GRID_PAGE_SIZE;
    renderTable('grid-result', value.columns ?? [], rows.slice(0, GRID_PAGE_SIZE), null);
    const first = rows.length ? offset + 1 : 0;
    const last = offset + Math.min(rows.length, GRID_PAGE_SIZE);
    $('grid-page').textContent = rows.length ? `${first}–${last}${gridState.hasMore ? '+' : ''}` : 'No rows';
    $('grid-prev').disabled = offset === 0;
    $('grid-next').disabled = !gridState.hasMore;
  } catch (error) { showError('grid-result', error); $('grid-page').textContent = ''; }
}
async function selectDatabase(name, tab) {
  const detail = $('database-detail');
  const generation = ++detailGeneration;
  selectedDatabase = name;
  detailTab = tab;
  gridState = { table: null, offset: 0, hasMore: false };
  detail.setAttribute('aria-busy', 'true');
  $('detail-state').hidden = false; $('detail-state').className = 'state state-loading'; $('detail-state').textContent = `Loading ${name}…`;
  $('detail-content').hidden = true;
  try {
    const data = await get(`/api/databases/${encodeURIComponent(name)}`);
    if (generation !== detailGeneration) return;
    renderDetail(data, tab);
  }
  catch (error) {
    if (generation !== detailGeneration) return;
    $('detail-state').className = 'state state-error'; $('detail-state').textContent = error.message;
  }
  finally { if (generation === detailGeneration) detail.setAttribute('aria-busy', 'false'); }
}
function setOptions(id, names) { $(id).innerHTML = names.map((name) => `<option value="${esc(name)}">${esc(name)}</option>`).join(''); }
function renderUsage(overview) {
  const gateway = overview.gateway || {};
  const requests = gateway.requests || {};
  const storage = enrichedDatabases.reduce((sum, db) => sum + (Number.isFinite(db.size_bytes) ? db.size_bytes : 0), 0);
  // An unknown usage is not a zero: a failed /v1/status emits null, which the
  // database table renders as unavailable, so the totals must agree with it.
  const usageUnknown = enrichedDatabases.some((db) => db.rows_read == null || db.rows_written == null);
  const reads = usageUnknown ? 'unavailable' : enrichedDatabases.reduce((sum, db) => sum + (db.rows_read ?? 0), 0);
  const writes = usageUnknown ? 'unavailable' : enrichedDatabases.reduce((sum, db) => sum + (db.rows_written ?? 0), 0);
  $('usage-panel').innerHTML = [['Databases', databaseNames.length], ['Storage', fmtBytes(storage)], ['Rows read', reads], ['Rows written', writes], ['Requests', requests.total ?? 'unavailable'], ['Uptime', Number.isFinite(gateway.uptime_secs) ? `${gateway.uptime_secs}s` : 'unavailable']].map(([key, value]) => `<div class="usage-row"><span>${esc(key)}</span><strong>${esc(value)}</strong></div>`).join('');
}
async function refresh() {
  const main = $('console-main');
  if (pending.get(main)) return;
  pending.set(main, true); main.setAttribute('aria-busy', 'true'); $('refresh').disabled = true;
  try {
    const [overview, enriched] = await Promise.all([get('/api/overview'), get('/api/databases')]);
    overviewCache = overview;
    enrichedDatabases = Array.isArray(enriched) ? enriched : [];
    const gateway = overview.gateway || {}; const requests = gateway.requests || {}; const admission = gateway.admission || {}; const tunnel = overview.tunnel || { state: 'not_configured' };
    const cards = [['Gateway', gateway.available ? 'reachable' : 'unavailable'], ['Version', gateway.version ?? 'unavailable'], ['Uptime', Number.isFinite(gateway.uptime_secs) ? `${gateway.uptime_secs}s` : 'unavailable'], ['Requests', requests.total ?? 'unavailable'], ['Capacity errors', requests.capacity_exceeded ?? 'unavailable'], ['Deadline errors', requests.query_deadline_exceeded ?? 'unavailable'], ['Internal errors', requests.internal_error ?? 'unavailable'], ['Admission timeout', admission.acquire_timeout_ms != null ? `${admission.acquire_timeout_ms} ms` : 'unavailable'], ['Read permits', admission.read_permits ?? 'unavailable'], ['Write permits', admission.write_permits ?? 'unavailable'], ['Tunnel', tunnel.state ?? 'not_configured'], ['Databases', Array.isArray(overview.databases) ? overview.databases.length : 'unavailable']];
    $('summary').innerHTML = cards.map(([key, value]) => `<article><b>${esc(key)}</b><strong>${esc(value)}</strong></article>`).join('');
    $('signals').innerHTML = (overview.resource_signals || []).map((signal) => `<article><b>${esc(signal.name)}</b><strong>${esc(signal.state)}</strong><small>${esc(signal.reason || '')}</small></article>`).join('') || '<p class="muted">No resource collector configured.</p>';
    databaseNames = Array.isArray(overview.databases) ? overview.databases : [];
    $('database-count').textContent = `${databaseNames.length} live database${databaseNames.length === 1 ? '' : 's'}`;
    renderDbTable();
    renderUsage(overview);
    renderBackups(overview.backups);
    $('tunnel-status').textContent = `Tunnel: ${tunnel.state ?? 'not_configured'}${tunnel.error ? ` — ${tunnel.error}` : ''}`;
    $('connection-status').textContent = gateway.available ? 'Gateway reachable through the loopback companion.' : 'Gateway unavailable; controls remain available for retry.';
    const route = parseHash();
    showView(route);
    if (route.view === 'detail' && route.name) {
      if (route.name !== selectedDatabase || !detailCache) await selectDatabase(route.name, route.tab);
      else { detailTab = route.tab; renderDetail(detailCache, route.tab); }
    }
  } catch (error) { $('connection-status').textContent = `Connection failed: ${error.message}`; $('signals').innerHTML = '<p class="state state-error">Signals unavailable.</p>'; }
  finally { pending.set(main, false); main.setAttribute('aria-busy', 'false'); $('refresh').disabled = false; }
}

$('refresh').addEventListener('click', refresh);
document.querySelectorAll('nav [data-view]').forEach((button) => button.addEventListener('click', () => navigate(button.dataset.view)));
$('create-open').addEventListener('click', () => { navigate('databases'); $('new-db').focus(); });
$('detail-back').addEventListener('click', () => navigate('databases'));
document.querySelectorAll('[role="tab"]').forEach((tab) => tab.addEventListener('click', () => {
  if (!selectedDatabase) return;
  // Apply tab selection and panel visibility synchronously. The hash update
  // below only reaches showView through the asynchronous `hashchange`
  // event, so without this the clicked tab keeps aria-selected=false until
  // that event lands — visible to keyboard and screen-reader users, and a
  // race for anything that reads the state right after the click.
  detailTab = tab.dataset.tab;
  showView({ view: 'detail', tab: detailTab });
  navigate('detail', selectedDatabase, detailTab);
}));
window.addEventListener('hashchange', () => { const route = parseHash(); showView(route); if (route.view === 'detail' && route.name && route.name !== selectedDatabase) void selectDatabase(route.name, route.tab); else if (route.view === 'detail' && route.name) { detailTab = route.tab; if (detailCache) renderDetail(detailCache, route.tab); } });
$('grid-table').addEventListener('change', () => { gridState = { table: $('grid-table').value || null, offset: 0, hasMore: false }; void loadGridPage(); });
$('grid-prev').addEventListener('click', () => { gridState.offset = Math.max(0, gridState.offset - GRID_PAGE_SIZE); void loadGridPage(); });
$('grid-next').addEventListener('click', () => { if (gridState.hasMore) { gridState.offset += GRID_PAGE_SIZE; void loadGridPage(); } });
$('tunnel-start').addEventListener('click', async (event) => { const button = event.currentTarget; button.disabled = true; try { await post('/api/tunnel/start', {}); await refresh(); } catch (error) { setText('tunnel-status', `Tunnel: ${error.message}`); } finally { button.disabled = false; } });
$('tunnel-stop').addEventListener('click', async (event) => { const button = event.currentTarget; button.disabled = true; try { await post('/api/tunnel/stop', {}); await refresh(); } catch (error) { setText('tunnel-status', `Tunnel: ${error.message}`); } finally { button.disabled = false; } });
$('query-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; pushConsoleHistory('sql', $('sql').value); runForm(form, 'Running query…', async () => { const headers = $('idempotency-key').value ? { 'idempotency-key': $('idempotency-key').value } : {}; const value = await post(`/api/databases/${encodeURIComponent($('database').value)}/query`, { sql: $('sql').value, args: [] }, headers); renderTable('result', value.columns ?? [], value.rows ?? [], value.rows_affected); }); });
$('batch-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; pushConsoleHistory('batch-json', $('batch-json').value); runForm(form, 'Running atomic batch…', async () => { const payload = JSON.parse($('batch-json').value); const statements = Array.isArray(payload) ? payload : payload.statements; const atomic = Array.isArray(payload) ? true : payload.atomic; if (!Array.isArray(statements) || atomic !== true) throw new Error('Batch JSON must contain atomic=true and a statements array.'); const headers = $('batch-idempotency-key').value ? { 'idempotency-key': $('batch-idempotency-key').value } : {}; const value = await post(`/api/databases/${encodeURIComponent($('batch-database').value)}/batch`, { atomic: true, statements }, headers); renderBatchResults('batch-result', value.results); }); });
$('create-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Creating database…', async () => { await post('/api/databases', { name: $('new-db').value }); await refresh(); }); });
$('delete-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; const name = $('delete-db').value; if (name !== $('confirm-db').value) { showError('result-actions', { code: 'confirmation_required', message: 'Type the exact database name to confirm deletion.' }); return; } if (!window.confirm(`Tombstone ${name}? This prevents queries until retention purge.`)) { setText('lifecycle-result', 'Deletion cancelled.'); return; } runForm(form, 'Deleting database…', async () => { $('result-actions').textContent = JSON.stringify(await request(`/api/databases/${encodeURIComponent(name)}`, { method: 'DELETE' }), null, 2); await refresh(); }); });
$('key-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Issuing one-time key…', async () => { const value = await post('/api/keys', { name: $('key-name').value, role: $('key-role').value, databases: [$('key-db').value] }); $('key-result').textContent = JSON.stringify(value, null, 2); await refresh(); }); });
$('clone-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; const source = $('clone-source').value; const destination = $('clone-destination').value; if (databaseNames.includes(destination)) { showError('lifecycle-result', { code: 'database_exists', message: 'Choose a new destination; existing databases are never overwritten.' }); return; } if (!window.confirm(`Clone ${source} to new database ${destination}?`)) { setText('lifecycle-result', 'Clone cancelled.'); return; } runForm(form, 'Cloning database…', async () => { $('lifecycle-result').textContent = JSON.stringify(await post(`/api/databases/${encodeURIComponent(source)}/clone`, { destination }), null, 2); await refresh(); }); });
$('export-form').addEventListener('submit', (event) => { event.preventDefault(); void runExport(); });
$('import-form').addEventListener('submit', (event) => { event.preventDefault(); void runImport(); });
wireConsoleInput('sql', 'query-form');
wireConsoleInput('batch-json', 'batch-form');

function renderTransfer(target, value, actions = []) { const output = $(target); output.textContent = JSON.stringify(value, null, 2); for (const [label, handler] of actions) { const button = document.createElement('button'); button.type = 'button'; button.textContent = label; button.addEventListener('click', handler, { once: true }); output.appendChild(button); } }
async function runExport() {
  const form = $('export-form'); if (pending.get(form)) return;
  const databaseName = $('export-db').value;
  if (!databaseName) { showError('export-result', { code: 'invalid_request', message: 'Choose a database first.' }); return; }
  if (exportTransfer && exportTransfer.database !== databaseName) {
    await request(`/api/snapshots/${encodeURIComponent(exportTransfer.session.snapshot)}`, { method: 'DELETE' }).catch(() => undefined);
    exportTransfer = null;
  }
  pending.set(form, true); setFormBusy(form, true, 'Exporting verified chunks…');
  try {
    if (!exportTransfer) exportTransfer = { database: databaseName, session: await post(`/api/databases/${encodeURIComponent(databaseName)}/export-begin`, {}), offset: 0, chunks: [] };
    const { session } = exportTransfer;
    while (exportTransfer.offset < session.size) { const offset = exportTransfer.offset; const length = Math.min(TRANSFER_CHUNK_BYTES, session.size - offset); const chunk = await get(`/api/snapshots/${encodeURIComponent(session.snapshot)}/chunk?offset=${offset}&length=${length}`); if (chunk.size !== session.size || chunk.sha256 !== session.sha256 || chunk.offset !== offset) throw new Error('Snapshot metadata changed during transfer'); const part = Uint8Array.from(atob(chunk.data_base64), (char) => char.charCodeAt(0)); if (!part.length || part.length > length || offset + part.length > session.size) throw new Error('Malformed snapshot chunk'); exportTransfer.chunks.push(part); exportTransfer.offset += part.length; renderTransfer('export-result', { database: databaseName, session: session.snapshot, transferred: exportTransfer.offset, total: session.size, transfer: 'chunked', state: 'in_progress' }); }
    const bytes = new Uint8Array(exportTransfer.offset); let cursor = 0; for (const chunk of exportTransfer.chunks) { bytes.set(chunk, cursor); cursor += chunk.length; }
    const digest = await crypto.subtle.digest('SHA-256', bytes); const sha256 = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join(''); if (sha256 !== session.sha256) throw new Error('Export integrity mismatch');
    await request(`/api/snapshots/${encodeURIComponent(session.snapshot)}`, { method: 'DELETE' }).catch(() => undefined); exportTransfer = null; renderTransfer('export-result', { database: databaseName, size: session.size, sha256: session.sha256, transfer: 'chunked', state: 'complete' });
    // A blob URL pins its buffer for the document's lifetime and an export
    // can be up to 128 MiB, so revoke the previous link and this one on use.
    if (exportBlobUrl) URL.revokeObjectURL(exportBlobUrl);
    const url = URL.createObjectURL(new Blob([bytes], { type: 'application/vnd.sqlite3' })); exportBlobUrl = url; const link = document.createElement('a'); link.href = url; link.download = `${databaseName}.sqlite`; link.textContent = 'Download verified SQLite export'; link.addEventListener('click', () => { URL.revokeObjectURL(url); exportBlobUrl = null; }, { once: true }); $('export-result').appendChild(link);
  } catch (error) { if (!exportTransfer) showError('export-result', error); else { const paused = exportTransfer; renderTransfer('export-result', { database: paused.database, session: paused.session.snapshot, transferred: paused.offset, total: paused.session.size, state: 'paused', error: { code: error.code || 'console_error', message: error.message } }, [['Resume export', () => runExport()], ['Abort export', async () => { try { await request(`/api/snapshots/${encodeURIComponent(paused.session.snapshot)}`, { method: 'DELETE' }); exportTransfer = null; renderTransfer('export-result', { session: paused.session.snapshot, state: 'aborted' }); } catch (abortError) { showError('export-result', abortError); } }]]); } }
  finally { pending.set(form, false); setFormBusy(form, false); }
}
async function runImport() {
  const form = $('import-form'); if (pending.get(form)) return; const file = $('import-file').files[0]; const name = $('import-name').value;
  if (!file || !name) { showError('import-result', { code: 'invalid_request', message: 'Choose a SQLite file and import name.' }); return; }
  if (file.size > MAX_TRANSFER_BYTES) { showError('import-result', { code: 'result_limit_exceeded', message: 'Import exceeds 128 MiB.' }); return; }
  if (databaseNames.includes(name)) { showError('import-result', { code: 'database_exists', message: 'Choose a new import name; existing databases are never overwritten.' }); return; }
  if (importTransfer && (importTransfer.file !== file || importTransfer.name !== name)) {
    await request(`/api/uploads/${encodeURIComponent(importTransfer.session.upload)}`, { method: 'DELETE' }).catch(() => undefined);
    importTransfer = null;
  }
  if (!window.confirm(`Import ${file.name} as new database ${name}?`)) { setText('import-result', 'Import cancelled.'); return; }
  pending.set(form, true); setFormBusy(form, true, 'Importing verified chunks…');
  try {
    if (!importTransfer) { const data = new Uint8Array(await file.arrayBuffer()); importTransfer = { file, name, data, session: await post('/api/databases/console/import-begin', {}), offset: 0 }; }
    const { data, session } = importTransfer; const status = await get(`/api/uploads/${encodeURIComponent(session.upload)}`); importTransfer.offset = status.received; renderTransfer('import-result', { upload: session.upload, transferred: importTransfer.offset, total: data.length, state: 'in_progress' });
    while (importTransfer.offset < data.length) { const offset = importTransfer.offset; const end = Math.min(offset + TRANSFER_CHUNK_BYTES, data.length); const encoded = await new Promise((resolve, reject) => { const reader = new FileReader(); reader.onload = () => resolve(String(reader.result).split(',')[1] || ''); reader.onerror = () => reject(new Error('Could not read the selected file.')); reader.readAsDataURL(new Blob([data.subarray(offset, end)], { type: 'application/octet-stream' })); }); const ack = await post(`/api/uploads/${encodeURIComponent(session.upload)}/chunk`, { offset, data_base64: encoded }); if (ack.received !== end) throw new Error('Upload acknowledgement mismatch'); importTransfer.offset = end; renderTransfer('import-result', { upload: session.upload, transferred: importTransfer.offset, total: data.length, state: 'in_progress' }); }
    $('import-result').textContent = JSON.stringify(await post(`/api/uploads/${encodeURIComponent(session.upload)}/commit`, { name }), null, 2); importTransfer = null; await refresh();
  } catch (error) { if (!importTransfer) showError('import-result', error); else { const paused = importTransfer; renderTransfer('import-result', { upload: paused.session.upload, transferred: paused.offset, total: paused.data.length, state: 'paused', error: { code: error.code || 'console_error', message: error.message } }, [['Resume import', () => runImport()], ['Abort upload', async () => { try { await request(`/api/uploads/${encodeURIComponent(paused.session.upload)}`, { method: 'DELETE' }); importTransfer = null; renderTransfer('import-result', { upload: paused.session.upload, state: 'aborted' }); } catch (abortError) { showError('import-result', abortError); } }]]); } }
  finally { pending.set(form, false); setFormBusy(form, false); }
}
if (!window.location.hash) window.location.hash = '#/databases';
refresh();
