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
$('export-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  try {
    const value = await post(`/api/databases/${encodeURIComponent($('export-db').value)}/export`, {});
    $('export-result').textContent = JSON.stringify({ database: value.database, size: value.size, sha256: value.sha256 }, null, 2);
    const bytes = Uint8Array.from(atob(value.data_base64), (char) => char.charCodeAt(0));
    const url = URL.createObjectURL(new Blob([bytes], { type: 'application/vnd.sqlite3' }));
    const link = document.createElement('a'); link.href = url; link.download = `${value.database}.sqlite`; link.textContent = 'Download verified SQLite export'; $('export-result').appendChild(link);
  } catch (error) { showError('export-result', error); }
});
$('import-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const file = $('import-file').files[0];
  if (!file) { showError('import-result', { message: 'Choose a SQLite file first.' }); return; }
  try { $('import-result').textContent = JSON.stringify(await post('/api/databases/console/import', { name: $('import-name').value, data_base64: await fileBase64(file), from: 'console' }), null, 2); await refresh(); } catch (error) { showError('import-result', error); }
});
refresh();
