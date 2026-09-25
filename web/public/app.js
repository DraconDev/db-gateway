const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[char]));

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
function show(value) { $('result').textContent = JSON.stringify(value, null, 2); }
function showError(target, error) {
  $(target).textContent = JSON.stringify({ error: { code: error.code || 'console_error', status: error.status, message: error.message } }, null, 2);
}
function fileBase64(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result).split(',')[1] || '');
    reader.onerror = () => reject(new Error('Could not read the selected file.'));
    reader.readAsDataURL(file);
  });
}

async function refresh() {
  try {
    const overview = await get('/api/overview');
    const gateway = overview.gateway || {};
    const requests = gateway.requests || {};
    const admission = gateway.admission || {};
    const tunnel = overview.tunnel || { state: 'not_configured' };
    const cards = [
      ['Gateway', gateway.available ? 'reachable' : 'unavailable'],
      ['Version', gateway.version ?? 'unavailable'],
      ['Uptime', Number.isFinite(gateway.uptime_secs) ? `${gateway.uptime_secs}s` : 'unavailable'],
      ['Requests', requests.total ?? 'unavailable'],
      ['Capacity errors', requests.capacity_exceeded ?? 'unavailable'],
      ['Deadline errors', requests.query_deadline_exceeded ?? 'unavailable'],
      ['Internal errors', requests.internal_error ?? 'unavailable'],
      ['Admission timeout', admission.acquire_timeout_ms != null ? `${admission.acquire_timeout_ms} ms` : 'unavailable'],
      ['Read permits', admission.read_permits ?? 'unavailable'],
      ['Write permits', admission.write_permits ?? 'unavailable'],
      ['Tunnel', tunnel.state ?? 'not_configured'],
      ['Databases', Array.isArray(overview.databases) ? overview.databases.length : 'unavailable'],
    ];
    $('summary').innerHTML = cards.map(([key, value]) => `<article><b>${esc(key)}</b><strong>${esc(value)}</strong></article>`).join('');
    $('signals').innerHTML = (overview.resource_signals || []).map((signal) => `<article><b>${esc(signal.name)}</b><strong>${esc(signal.state)}</strong><small>${esc(signal.reason || '')}</small></article>`).join('') || '<p>No resource collector configured.</p>';

    const names = Array.isArray(overview.databases) ? overview.databases : [];
    $('databases').innerHTML = names.map((name) => `<button class="db" data-name="${esc(name)}">${esc(name)}</button>`).join('') || '<p>No databases reported.</p>';
    document.querySelectorAll('.db').forEach((button) => button.addEventListener('click', async () => {
      try { show(await get(`/api/databases/${encodeURIComponent(button.dataset.name)}`)); } catch (error) { showError('result', error); }
    }));
    const options = names.map((name) => `<option value="${esc(name)}">${esc(name)}</option>`).join('');
    $('database').innerHTML = options;
    $('batch-database').innerHTML = options;
    $('clone-source').innerHTML = options;
    $('export-db').innerHTML = options;
    $('backups').textContent = JSON.stringify(overview.backups || { state: 'unknown' }, null, 2);
    $('tunnel-status').textContent = `Tunnel: ${tunnel.state ?? 'not_configured'}${tunnel.error ? ` — ${tunnel.error}` : ''}`;

    const keys = Array.isArray(overview.keys) ? overview.keys : [];
    $('key-list').innerHTML = keys.length ? keys.map((key) => `<article><b>${esc(key.name)}</b><strong>${esc(key.role)}</strong><small>${esc((key.databases || []).join(', '))}</small>${key.name === 'owner' ? '' : `<button data-revoke="${esc(key.name)}">Revoke</button>`}</article>`).join('') : '<p>No scoped keys reported.</p>';
    document.querySelectorAll('[data-revoke]').forEach((button) => button.addEventListener('click', async () => {
      if (!window.confirm(`Revoke key ${button.dataset.revoke}?`)) return;
      try { await request(`/api/keys/${encodeURIComponent(button.dataset.revoke)}`, { method: 'DELETE' }); await refresh(); } catch (error) { showError('key-result', error); }
    }));
  } catch (error) {
    $('summary').innerHTML = `<article class="error"><b>Unavailable</b><strong>${esc(error.message)}</strong></article>`;
    $('signals').textContent = 'Signals unavailable';
  }
}

$('refresh').addEventListener('click', refresh);
$('tunnel-start').addEventListener('click', async () => { try { await post('/api/tunnel/start', {}); await refresh(); } catch (error) { $('tunnel-status').textContent = `Tunnel: ${error.message}`; } });
$('tunnel-stop').addEventListener('click', async () => { try { await post('/api/tunnel/stop', {}); await refresh(); } catch (error) { $('tunnel-status').textContent = `Tunnel: ${error.message}`; } });
$('query-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const headers = $('idempotency-key').value ? { 'idempotency-key': $('idempotency-key').value } : {};
    show(await post(`/api/databases/${encodeURIComponent($('database').value)}/query`, { sql: $('sql').value, args: [] }, headers));
  } catch (error) { showError('result', error); }
});
$('batch-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const statements = JSON.parse($('batch-json').value);
    const headers = $('batch-idempotency-key').value ? { 'idempotency-key': $('batch-idempotency-key').value } : {};
    $('batch-result').textContent = JSON.stringify(await post(`/api/databases/${encodeURIComponent($('batch-database').value)}/batch`, { atomic: true, statements }, headers), null, 2);
  } catch (error) { showError('batch-result', error); }
});
$('create-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try { show(await post('/api/databases', { name: $('new-db').value })); await refresh(); } catch (error) { showError('result', error); }
});
$('delete-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const name = $('delete-db').value;
  if (name !== $('confirm-db').value) { show({ error: 'Names do not match' }); return; }
  try { show(await request(`/api/databases/${encodeURIComponent(name)}`, { method: 'DELETE' })); await refresh(); } catch (error) { showError('result', error); }
});
$('key-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try { $('key-result').textContent = JSON.stringify(await post('/api/keys', { name: $('key-name').value, role: $('key-role').value, databases: [$('key-db').value] }), null, 2); await refresh(); } catch (error) { showError('key-result', error); }
});
$('clone-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try { $('lifecycle-result').textContent = JSON.stringify(await post(`/api/databases/${encodeURIComponent($('clone-source').value)}/clone`, { destination: $('clone-destination').value }), null, 2); await refresh(); } catch (error) { showError('lifecycle-result', error); }
});
const MAX_TRANSFER_BYTES = 128 * 1024 * 1024;
const TRANSFER_CHUNK_BYTES = 1024 * 1024;
let exportTransfer = null;
let importTransfer = null;

function renderTransfer(target, value, actions = []) {
  const output = $(target);
  output.textContent = JSON.stringify(value, null, 2);
  for (const [label, handler] of actions) {
    const button = document.createElement('button');
    button.type = 'button';
    button.textContent = label;
    button.addEventListener('click', handler, { once: true });
    output.appendChild(button);
  }
}

async function runExport() {
  const databaseName = $('export-db').value;
  if (!databaseName) { showError('export-result', { code: 'invalid_request', message: 'Choose a database first.' }); return; }
  if (exportTransfer && exportTransfer.database !== databaseName) {
    await request(`/api/snapshots/${encodeURIComponent(exportTransfer.session.snapshot)}`, { method: 'DELETE' }).catch(() => undefined);
    exportTransfer = null;
  }
  try {
    if (!exportTransfer) {
      const session = await post(`/api/databases/${encodeURIComponent(databaseName)}/export-begin`, {});
      exportTransfer = { database: databaseName, session, offset: 0, chunks: [] };
    }
    const { session } = exportTransfer;
    while (exportTransfer.offset < session.size) {
      const offset = exportTransfer.offset;
      const length = Math.min(TRANSFER_CHUNK_BYTES, session.size - offset);
      const chunk = await get(`/api/snapshots/${encodeURIComponent(session.snapshot)}/chunk?offset=${offset}&length=${length}`);
      if (chunk.size !== session.size || chunk.sha256 !== session.sha256 || chunk.offset !== offset) throw new Error('Snapshot metadata changed during transfer');
      const part = Uint8Array.from(atob(chunk.data_base64), (char) => char.charCodeAt(0));
      if (!part.length || part.length > length || offset + part.length > session.size) throw new Error('Malformed snapshot chunk');
      exportTransfer.chunks.push(part);
      exportTransfer.offset += part.length;
      renderTransfer('export-result', { database: databaseName, session: session.snapshot, transferred: exportTransfer.offset, total: session.size, sha256: session.sha256, transfer: 'chunked', state: 'in_progress' });
    }
    const bytes = new Uint8Array(exportTransfer.offset); let cursor = 0;
    for (const chunk of exportTransfer.chunks) { bytes.set(chunk, cursor); cursor += chunk.length; }
    const digest = await crypto.subtle.digest('SHA-256', bytes);
    const sha256 = [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
    if (sha256 !== session.sha256) throw new Error('Export integrity mismatch');
    await request(`/api/snapshots/${encodeURIComponent(session.snapshot)}`, { method: 'DELETE' }).catch(() => undefined);
    exportTransfer = null;
    renderTransfer('export-result', { database: databaseName, size: session.size, sha256: session.sha256, transfer: 'chunked', state: 'complete' });
    const url = URL.createObjectURL(new Blob([bytes], { type: 'application/vnd.sqlite3' }));
    const link = document.createElement('a'); link.href = url; link.download = `${databaseName}.sqlite`; link.textContent = 'Download verified SQLite export'; $('export-result').appendChild(link);
  } catch (error) {
    if (!exportTransfer) { showError('export-result', error); return; }
    const paused = exportTransfer;
    renderTransfer('export-result', { database: paused.database, session: paused.session.snapshot, transferred: paused.offset, total: paused.session.size, transfer: 'chunked', state: 'paused', error: { code: error.code || 'console_error', message: error.message }, resume: 'Resume uses the retained snapshot and verified offset.' }, [
      ['Resume export', () => runExport()],
      ['Abort export', async () => { try { await request(`/api/snapshots/${encodeURIComponent(paused.session.snapshot)}`, { method: 'DELETE' }); exportTransfer = null; renderTransfer('export-result', { session: paused.session.snapshot, state: 'aborted' }); } catch (abortError) { showError('export-result', abortError); } }],
    ]);
  }
}

async function runImport() {
  const file = $('import-file').files[0];
  const name = $('import-name').value;
  if (!file) { showError('import-result', { code: 'invalid_request', message: 'Choose a SQLite file first.' }); return; }
  if (!name) { showError('import-result', { code: 'invalid_request', message: 'Choose an import name.' }); return; }
  if (file.size > MAX_TRANSFER_BYTES) { showError('import-result', { code: 'result_limit_exceeded', message: 'Import exceeds 128 MiB.' }); return; }
  if (importTransfer && (importTransfer.file !== file || importTransfer.name !== name)) {
    await request(`/api/uploads/${encodeURIComponent(importTransfer.session.upload)}`, { method: 'DELETE' }).catch(() => undefined);
    importTransfer = null;
  }
  try {
    if (!importTransfer) {
      const data = new Uint8Array(await file.arrayBuffer());
      const session = await post('/api/databases/console/import-begin', {});
      importTransfer = { file, name, data, session, offset: 0 };
    }
    const { data, session } = importTransfer;
    const status = await get(`/api/uploads/${encodeURIComponent(session.upload)}`);
    if (status.received > data.length) throw new Error('Server received more bytes than the selected file');
    importTransfer.offset = status.received;
    renderTransfer('import-result', { upload: session.upload, transferred: importTransfer.offset, total: data.length, state: 'in_progress' });
    while (importTransfer.offset < data.length) {
      const offset = importTransfer.offset;
      const end = Math.min(offset + TRANSFER_CHUNK_BYTES, data.length);
      const encoded = await new Promise((resolve, reject) => { const reader = new FileReader(); reader.onload = () => resolve(String(reader.result).split(',')[1] || ''); reader.onerror = () => reject(new Error('Could not read the selected file.')); reader.readAsDataURL(new Blob([data.subarray(offset, end)], { type: 'application/octet-stream' })); });
      const acknowledged = await post(`/api/uploads/${encodeURIComponent(session.upload)}/chunk`, { offset, data_base64: encoded });
      if (acknowledged.received !== end) throw new Error('Upload acknowledgement mismatch');
      importTransfer.offset = end;
      renderTransfer('import-result', { upload: session.upload, transferred: importTransfer.offset, total: data.length, state: 'in_progress' });
    }
    const done = await post(`/api/uploads/${encodeURIComponent(session.upload)}/commit`, { name });
    importTransfer = null;
    renderTransfer('import-result', { ...done, state: 'complete' });
    await refresh();
  } catch (error) {
    if (!importTransfer) { showError('import-result', error); return; }
    const paused = importTransfer;
    renderTransfer('import-result', { upload: paused.session.upload, transferred: paused.offset, total: paused.data.length, state: 'paused', error: { code: error.code || 'console_error', message: error.message }, resume: 'Resume reads the server offset before sending the next chunk.' }, [
      ['Resume import', () => runImport()],
      ['Abort upload', async () => { try { await request(`/api/uploads/${encodeURIComponent(paused.session.upload)}`, { method: 'DELETE' }); importTransfer = null; renderTransfer('import-result', { upload: paused.session.upload, state: 'aborted' }); } catch (abortError) { showError('import-result', abortError); } }],
    ]);
  }
}

$('export-form').addEventListener('submit', (event) => { event.preventDefault(); void runExport(); });
$('import-form').addEventListener('submit', (event) => { event.preventDefault(); void runImport(); });
refresh();
