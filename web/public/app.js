const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]));
const MAX_TRANSFER_BYTES = 128 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES = 1024 * 1024;
let selectedDatabase = null;
let databaseNames = [];
let exportTransfer = null;
let importTransfer = null;
const pending = new WeakMap();

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
  try { await work(); }
  catch (error) { showError('result', error); }
  finally { pending.set(form, false); setFormBusy(form, false); }
}
function show(value) { $('result').textContent = JSON.stringify(value, null, 2); }
function showError(target, error) {
  const element = $(target);
  if (element) element.textContent = JSON.stringify({ error: { code: error.code || 'console_error', status: error.status, message: error.message } }, null, 2);
}
function setText(target, value) { const element = $(target); if (element) element.textContent = value; }
function renderTable(target, columns, rows, rowsAffected) {
  const element = $(target);
  if (!element) return;
  const affected = rowsAffected == null ? '' : `<p class="muted">Rows affected: ${esc(rowsAffected)}</p>`;
  if (!columns.length) { element.innerHTML = `${affected}<p class="muted">No columns returned.</p>`; return; }
  element.innerHTML = `${affected}<div class="table-wrap"><table><thead><tr>${columns.map((column) => `<th scope="col">${esc(column)}</th>`).join('')}</tr></thead><tbody>${rows.map((row) => `<tr>${columns.map((_, index) => `<td>${esc(row[index])}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}
function renderBackups(backups) {
  const entries = Object.entries(backups?.per_database ?? {});
  $('backups-summary').innerHTML = entries.length ? entries.map(([name, value]) => `<article><b>${esc(name)}</b><strong>${esc(value.state)}</strong><small>${value.age_secs == null ? 'Age unavailable' : `${value.age_secs}s old`}</small></article>`).join('') : '<p class="muted">No database freshness records reported.</p>';
  $('backups').textContent = JSON.stringify(backups ?? { state: 'unknown' }, null, 2);
}
function renderDetail(detail) {
  const state = $('detail-state');
  const content = $('detail-content');
  state.hidden = true; state.className = 'state state-empty';
  content.hidden = false;
  const tables = Array.isArray(detail.tables) ? detail.tables : [];
  const schemaRows = detail.schema?.rows ?? [];
  content.innerHTML = `<div class="section-heading"><h3>${esc(detail.name)}</h3><span class="status-pill">${detail.tombstoned ? 'tombstoned' : 'live'}</span></div><p class="muted">${detail.size_bytes} bytes · created ${detail.created_at ?? 'unknown'}</p><h4>Table statistics</h4>${tables.length ? `<div class="table-wrap"><table><thead><tr><th>Table</th><th>Rows</th></tr></thead><tbody>${tables.map((table) => `<tr><td>${esc(table.name)}</td><td>${esc(table.rows)}</td></tr>`).join('')}</tbody></table></div>` : '<p class="muted">No user tables reported.</p>'}<h4>Schema</h4>${schemaRows.length ? schemaRows.map((row) => `<details><summary>${esc(row[0])}</summary><pre>${esc(row[1] ?? 'Schema unavailable')}</pre></details>`).join('') : '<p class="muted">No schema statements reported.</p>'}`;
}
async function selectDatabase(name) {
  const detail = $('database-detail');
  selectedDatabase = name;
  detail.setAttribute('aria-busy', 'true');
  $('detail-state').hidden = false; $('detail-state').className = 'state state-loading'; $('detail-state').textContent = `Loading ${name}…`;
  $('detail-content').hidden = true;
  try { renderDetail(await get(`/api/databases/${encodeURIComponent(name)}`)); }
  catch (error) { $('detail-state').className = 'state state-error'; $('detail-state').textContent = error.message; }
  finally { detail.setAttribute('aria-busy', 'false'); }
}
function setOptions(id, names) { $(id).innerHTML = names.map((name) => `<option value="${esc(name)}">${esc(name)}</option>`).join(''); }
async function refresh() {
  const main = $('console-main');
  if (pending.get(main)) return;
  pending.set(main, true); main.setAttribute('aria-busy', 'true'); $('refresh').disabled = true;
  try {
    const overview = await get('/api/overview');
    const gateway = overview.gateway || {}; const requests = gateway.requests || {}; const admission = gateway.admission || {}; const tunnel = overview.tunnel || { state: 'not_configured' };
    const cards = [['Gateway', gateway.available ? 'reachable' : 'unavailable'], ['Version', gateway.version ?? 'unavailable'], ['Uptime', Number.isFinite(gateway.uptime_secs) ? `${gateway.uptime_secs}s` : 'unavailable'], ['Requests', requests.total ?? 'unavailable'], ['Capacity errors', requests.capacity_exceeded ?? 'unavailable'], ['Deadline errors', requests.query_deadline_exceeded ?? 'unavailable'], ['Internal errors', requests.internal_error ?? 'unavailable'], ['Admission timeout', admission.acquire_timeout_ms != null ? `${admission.acquire_timeout_ms} ms` : 'unavailable'], ['Read permits', admission.read_permits ?? 'unavailable'], ['Write permits', admission.write_permits ?? 'unavailable'], ['Tunnel', tunnel.state ?? 'not_configured'], ['Databases', Array.isArray(overview.databases) ? overview.databases.length : 'unavailable']];
    $('summary').innerHTML = cards.map(([key, value]) => `<article><b>${esc(key)}</b><strong>${esc(value)}</strong></article>`).join('');
    $('signals').innerHTML = (overview.resource_signals || []).map((signal) => `<article><b>${esc(signal.name)}</b><strong>${esc(signal.state)}</strong><small>${esc(signal.reason || '')}</small></article>`).join('') || '<p class="muted">No resource collector configured.</p>';
    databaseNames = Array.isArray(overview.databases) ? overview.databases : [];
    $('database-count').textContent = `${databaseNames.length} live database${databaseNames.length === 1 ? '' : 's'}`;
    $('databases').innerHTML = databaseNames.map((name) => `<button class="db" data-name="${esc(name)}" aria-pressed="${name === selectedDatabase}">${esc(name)}</button>`).join('') || '<p class="muted">No databases reported.</p>';
    document.querySelectorAll('.db').forEach((button) => button.addEventListener('click', () => selectDatabase(button.dataset.name)));
    for (const id of ['database', 'batch-database', 'clone-source', 'export-db']) setOptions(id, databaseNames);
    renderBackups(overview.backups);
    $('tunnel-status').textContent = `Tunnel: ${tunnel.state ?? 'not_configured'}${tunnel.error ? ` — ${tunnel.error}` : ''}`;
    const keys = Array.isArray(overview.keys) ? overview.keys : [];
    $('key-list').innerHTML = keys.length ? keys.map((key) => `<article><b>${esc(key.name)}</b><strong>${esc(key.role)}</strong><small>${esc((key.databases || []).join(', '))}</small>${key.name === 'owner' ? '' : `<button data-revoke="${esc(key.name)}">Revoke</button>`}</article>`).join('') : '<p class="muted">No scoped keys reported.</p>';
    document.querySelectorAll('[data-revoke]').forEach((button) => button.addEventListener('click', async () => { if (!window.confirm(`Revoke key ${button.dataset.revoke}?`)) return; try { await request(`/api/keys/${encodeURIComponent(button.dataset.revoke)}`, { method: 'DELETE' }); await refresh(); } catch (error) { showError('key-result', error); } }));
    $('connection-status').textContent = gateway.available ? 'Gateway reachable through the loopback companion.' : 'Gateway unavailable; controls remain available for retry.';
  } catch (error) { $('connection-status').textContent = error.message; $('signals').innerHTML = '<p class="state state-error">Signals unavailable.</p>'; }
  finally { pending.set(main, false); main.setAttribute('aria-busy', 'false'); $('refresh').disabled = false; }
}

$('refresh').addEventListener('click', refresh);
$('tunnel-start').addEventListener('click', async (event) => { const button = event.currentTarget; button.disabled = true; try { await post('/api/tunnel/start', {}); await refresh(); } catch (error) { setText('tunnel-status', `Tunnel: ${error.message}`); } finally { button.disabled = false; } });
$('tunnel-stop').addEventListener('click', async (event) => { const button = event.currentTarget; button.disabled = true; try { await post('/api/tunnel/stop', {}); await refresh(); } catch (error) { setText('tunnel-status', `Tunnel: ${error.message}`); } finally { button.disabled = false; } });
$('query-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Running query…', async () => { const headers = $('idempotency-key').value ? { 'idempotency-key': $('idempotency-key').value } : {}; const value = await post(`/api/databases/${encodeURIComponent($('database').value)}/query`, { sql: $('sql').value, args: [] }, headers); renderTable('result', value.columns ?? [], value.rows ?? [], value.rows_affected); }); });
$('batch-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Running atomic batch…', async () => { const payload = JSON.parse($('batch-json').value); const statements = Array.isArray(payload) ? payload : payload.statements; const atomic = Array.isArray(payload) ? true : payload.atomic; if (!Array.isArray(statements) || atomic !== true) throw new Error('Batch JSON must contain atomic=true and a statements array.'); const headers = $('batch-idempotency-key').value ? { 'idempotency-key': $('batch-idempotency-key').value } : {}; const value = await post(`/api/databases/${encodeURIComponent($('batch-database').value)}/batch`, { atomic: true, statements }, headers); const first = value.results?.[0]; renderTable('batch-result', first?.columns ?? [], first?.rows ?? [], first?.rows_affected); }); });
$('create-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Creating database…', async () => { await post('/api/databases', { name: $('new-db').value }); await refresh(); }); });
$('delete-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; const name = $('delete-db').value; if (name !== $('confirm-db').value) { showError('result-actions', { code: 'confirmation_required', message: 'Type the exact database name to confirm deletion.' }); return; } if (!window.confirm(`Tombstone ${name}? This prevents queries until retention purge.`)) { setText('lifecycle-result', 'Deletion cancelled.'); return; } runForm(form, 'Deleting database…', async () => { show(await request(`/api/databases/${encodeURIComponent(name)}`, { method: 'DELETE' })); await refresh(); }); });
$('key-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; runForm(form, 'Issuing one-time key…', async () => { const value = await post('/api/keys', { name: $('key-name').value, role: $('key-role').value, databases: [$('key-db').value] }); $('key-result').textContent = JSON.stringify(value, null, 2); await refresh(); }); });
$('clone-form').addEventListener('submit', (event) => { event.preventDefault(); const form = event.currentTarget; const source = $('clone-source').value; const destination = $('clone-destination').value; if (databaseNames.includes(destination)) { showError('lifecycle-result', { code: 'database_exists', message: 'Choose a new destination; existing databases are never overwritten.' }); return; } if (!window.confirm(`Clone ${source} to new database ${destination}?`)) { setText('lifecycle-result', 'Clone cancelled.'); return; } runForm(form, 'Cloning database…', async () => { $('lifecycle-result').textContent = JSON.stringify(await post(`/api/databases/${encodeURIComponent(source)}/clone`, { destination }), null, 2); await refresh(); }); });
$('export-form').addEventListener('submit', (event) => { event.preventDefault(); void runExport(); });
$('import-form').addEventListener('submit', (event) => { event.preventDefault(); void runImport(); });

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
    const url = URL.createObjectURL(new Blob([bytes], { type: 'application/vnd.sqlite3' })); const link = document.createElement('a'); link.href = url; link.download = `${databaseName}.sqlite`; link.textContent = 'Download verified SQLite export'; $('export-result').appendChild(link);
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
refresh();
