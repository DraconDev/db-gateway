const $ = (id) => document.getElementById(id);
async function get(path) { const r = await fetch(path); const v = await r.json(); if (!r.ok) throw new Error(v.error?.message || 'request failed'); return v; }
async function post(path, value) { const r = await fetch(path, { method: 'POST', headers: {'content-type':'application/json'}, body: JSON.stringify(value) }); const v = await r.json(); if (!r.ok) throw new Error(v.error?.message || 'request failed'); return v; }
function show(value) { $('result').textContent = JSON.stringify(value, null, 2); }
async function refresh() {
  try {
    const [overview, backups] = await Promise.all([get('/api/overview'), get('/api/backups')]);
    const s = overview.gateway;
    $('summary').innerHTML = [['Gateway', s.available ? 'reachable' : 'unavailable'], ['Version', s.version], ['Uptime', `${s.uptime_secs}s`], ['Requests', s.requests.total], ['Capacity errors', s.requests.capacity_exceeded], ['Databases', overview.databases.length]].map(([k,v]) => `<article><b>${k}</b><strong>${v}</strong></article>`).join('');
    $('databases').innerHTML = overview.databases.map((name) => `<button class="db" data-name="${name}">${name}</button>`).join('') || '<p>No databases reported.</p>';
    document.querySelectorAll('.db').forEach((b) => b.addEventListener('click', async () => { try { show(await get(`/api/databases/${encodeURIComponent(b.dataset.name)}`)); } catch (e) { show({ error: e.message }); } }));
    $('database').innerHTML = overview.databases.map((name) => `<option>${name}</option>`).join('');
    const signals = overview.resource_signals || [];
    $('signals').innerHTML = signals.length ? signals.map((x) => `<article><b>${x.name}</b><strong>${x.state}</strong><small>${x.reason || ''}</small></article>`).join('') : '<p>No resource collector configured.</p>';
    $('backups').textContent = JSON.stringify(backups, null, 2);
  } catch (e) { $('summary').innerHTML = `<article class="error"><b>Unavailable</b><strong>${e.message}</strong></article>`; $('signals').textContent = 'Signals unavailable'; }
}
$('refresh').addEventListener('click', refresh);
$('query-form').addEventListener('submit', async (e) => { e.preventDefault(); try { show(await post(`/api/databases/${encodeURIComponent($('database').value)}/query`, {sql: $('sql').value, args: []})); } catch (e) { show({ error: e.message }); } });
$('create-form').addEventListener('submit', async (e) => { e.preventDefault(); try { show(await post('/api/databases', {name: $('new-db').value})); await refresh(); } catch (e) { show({ error: e.message }); } });
$('delete-form').addEventListener('submit', async (e) => { e.preventDefault(); const name = $('delete-db').value; if (name !== $('confirm-db').value) { show({error:'Names do not match'}); return; } try { const r = await fetch(`/api/databases/${encodeURIComponent(name)}`, {method:'DELETE'}); show(await r.json()); await refresh(); } catch (e) { show({ error: e.message }); } });
$('key-form').addEventListener('submit', async (e) => { e.preventDefault(); try { const value = await post('/api/keys', {name: $('key-name').value, role: $('key-role').value, databases: [$('key-db').value]}); $('key-result').textContent = JSON.stringify(value, null, 2); } catch (e) { $('key-result').textContent = JSON.stringify({error:e.message}); } });
$('clone-form').addEventListener('submit', async (e) => { e.preventDefault(); try { $('lifecycle-result').textContent = JSON.stringify(await post(`/api/databases/${encodeURIComponent($('clone-source').value)}/clone`, {destination: $('clone-destination').value}), null, 2); await refresh(); } catch (e) { $('lifecycle-result').textContent = JSON.stringify({error:e.message}); } });
refresh();
