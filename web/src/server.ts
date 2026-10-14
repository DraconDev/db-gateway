import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { lstat, readFile } from 'node:fs/promises';
import { randomBytes } from 'node:crypto';
import { isIP } from 'node:net';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { ApiError, GatewayApi, redactCredential } from './api.js';
import { TunnelController, type TunnelOptions } from './tunnel.js';

const ROOT = dirname(fileURLToPath(import.meta.url));
const PUBLIC = join(ROOT, '..', 'public');
const MAX_BODY = 12 * 1024 * 1024;
const MAX_CHUNK_BODY = 2 * 1024 * 1024;
const LOOPBACK_NAMES = new Set(['localhost']);
const RESPONSE_CREDENTIALS = new WeakMap<ServerResponse, string>();

function env(name: string, fallback = ''): string { return process.env[name] ?? fallback; }
function isLoopback(host: string): boolean {
  if (typeof host !== 'string') return false;
  const normalized = host.trim().toLowerCase();
  if (LOOPBACK_NAMES.has(normalized)) return true;
  if (normalized === '::1' || normalized === '[::1]') return true;
  if (isIP(normalized) === 4) return normalized.split('.')[0] === '127';
  if (isIP(normalized) === 6) return normalized === '::1';
  return false;
}
function bindResponseCredential(res: ServerResponse, credential: string): void {
  RESPONSE_CREDENTIALS.set(res, credential);
}
export function logRequestFailure(error: unknown, credential: string): void {
  const message = error instanceof Error ? error.message : 'unknown error';
  console.error('console request failed', redactCredential(message, credential));
}
function json(res: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(redactCredential(value, RESPONSE_CREDENTIALS.get(res) ?? ''));
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
  res.end(body);
}
function text(res: ServerResponse, status: number, body: string, type = 'text/plain; charset=utf-8'): void {
  const safeBody = redactCredential(body, RESPONSE_CREDENTIALS.get(res) ?? '');
  res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
  res.end(safeBody);
}
async function body(req: IncomingMessage, limit = MAX_BODY): Promise<unknown> {
  let size = 0; const chunks: Buffer[] = [];
  for await (const chunk of req) {
    const b = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    size += b.length;
    if (size > limit) throw new ApiError('request_too_large', 'Request body is too large.', 413);
    chunks.push(b);
  }
  if (!chunks.length) return {};
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { throw new ApiError('invalid_request', 'Request body must be valid JSON.', 400); }
}
function pathParts(url: string): string[] {
  try {
    return new URL(url, 'http://console.local').pathname.split('/').filter(Boolean).map((part) => decodeURIComponent(part));
  } catch {
    throw new ApiError('invalid_request', 'Malformed request URL.', 400);
  }
}
function trustedOrigin(origin: string | undefined, config: ConsoleConfig): boolean {
  if (!origin) return true;
  try {
    const url = new URL(origin);
    return url.protocol === 'http:' && isLoopback(url.hostname) && (config.port === 0 || Number(url.port || 80) === config.port);
  } catch { return false; }
}

export interface ConsoleConfig {
  host: string;
  port: number;
  gatewayUrl: string;
  keyFile: string;
  tunnel?: TunnelOptions;
}
export function configFromEnv(): ConsoleConfig {
  const host = env('DG_CONSOLE_HOST', '127.0.0.1');
  if (!isLoopback(host)) throw new Error('DG_CONSOLE_HOST must be loopback');
  const port = Number(env('DG_CONSOLE_PORT', '8787'));
  if (!Number.isInteger(port) || port < 1024 || port > 65535) throw new Error('DG_CONSOLE_PORT must be 1024-65535');
  const target = env('DG_SSH_TARGET');
  const localPort = Number(env('DG_SSH_LOCAL_PORT', '17432'));
  if (!Number.isInteger(localPort) || localPort < 1024 || localPort > 65535) throw new Error('DG_SSH_LOCAL_PORT must be 1024-65535');
  const remotePort = Number(env('DG_SSH_REMOTE_PORT', '7432'));
  if (!Number.isInteger(remotePort) || remotePort < 1 || remotePort > 65535) throw new Error('DG_SSH_REMOTE_PORT must be 1-65535');
  const sshPortRaw = env('DG_SSH_PORT');
  const sshPort = sshPortRaw ? Number(sshPortRaw) : undefined;
  if (sshPort !== undefined && (!Number.isInteger(sshPort) || sshPort < 1 || sshPort > 65535)) throw new Error('DG_SSH_PORT must be 1-65535');
  const tunnel: TunnelOptions | undefined = target ? {
    target,
    localPort,
    remoteHost: env('DG_SSH_REMOTE_HOST', '127.0.0.1'),
    remotePort,
    sshPort,
    identityFile: env('DG_SSH_IDENTITY_FILE') || undefined,
    command: env('DG_SSH_COMMAND') || undefined,
  } : undefined;
  const gatewayUrl = env('DG_SERVER', tunnel ? `http://127.0.0.1:${localPort}` : 'http://127.0.0.1:7432');
  // DG_KEY_FILE is explicit when set (even to ''): an empty value is the
  // operator's "no key file" signal and must error out rather than fall
  // through to a default they may not have intended. An unset HOME must
  // leave the default empty so the guard below fires: join('',
  // '.config/dg/key') is the non-empty *relative* path '.config/dg/key',
  // which silently reads a key from the launch directory.
  const dgKeyFile = process.env.DG_KEY_FILE;
  let keyFile: string;
  if (dgKeyFile !== undefined) {
    if (dgKeyFile === '') throw new Error('DG_KEY_FILE is set to an empty value; unset it to use the default, or point it at a real file');
    keyFile = dgKeyFile;
  } else {
    keyFile = process.env.HOME ? join(process.env.HOME, '.config/dg/key') : '';
  }
  if (!keyFile) throw new Error('DG_KEY_FILE is required');
  return { host, port, gatewayUrl, keyFile, tunnel };
}

export async function readCredential(path: string): Promise<string> {
  // lstat (not stat): a symlink at `path` would let a planted entry
  // redirect the read to a different file between the mode check and the
  // open. The user's config dir is operator-owned, so the risk is
  // operator-introduced, but the TOCTOU closes for the same cost as the
  // permission check that follows.
  const metadata = await lstat(path);
  if (metadata.isSymbolicLink() || !metadata.isFile() || metadata.size > 4096) throw new Error('credential file must be a small regular file');
  if (process.platform !== 'win32' && (metadata.mode & 0o077) !== 0) throw new Error('credential file must not be accessible by group or others');
  const value = (await readFile(path, 'utf8')).trim();
  if (!/^[a-f0-9]{64}$/i.test(value)) throw new Error('credential file must contain one 64-character hex key');
  return value;
}

export async function serve(config: ConsoleConfig): Promise<{ close: () => Promise<void>; server: ReturnType<typeof createServer> }> {
  if (!isLoopback(config.host)) throw new Error('Console host must be loopback');
  const credential = await readCredential(config.keyFile);
  const api = new GatewayApi(config.gatewayUrl, credential);
  const session = sessionId();
  const tunnel = new TunnelController(config.tunnel);
  if (config.tunnel) await tunnel.start();
  const server = createServer(async (req, res) => {
    bindResponseCredential(res, credential);
    res.setHeader('x-content-type-options', 'nosniff');
    res.setHeader('x-frame-options', 'DENY');
    res.setHeader('referrer-policy', 'no-referrer');
    res.setHeader('content-security-policy', "default-src 'self'; connect-src 'self'; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'");
    try {
      const parts = pathParts(req.url ?? '/');
      if (!trustedOrigin(req.headers.origin, config)) throw new ApiError('invalid_origin', 'Cross-origin console requests are not allowed.', 403);
      const cookies = (req.headers.cookie ?? '').split(';').map((x) => x.trim());
      if (parts[0] === 'api' && !cookies.includes(`dg_console_session=${session}`)) throw new ApiError('console_session_required', 'Open the console in a browser session first.', 401);
      if (parts.length === 0 && req.method === 'GET') {
        res.setHeader('set-cookie', `dg_console_session=${session}; HttpOnly; SameSite=Strict; Path=/`);
        const html = await readFile(join(PUBLIC, 'index.html'), 'utf8'); text(res, 200, html, 'text/html; charset=utf-8'); return;
      }
      if (req.method === 'GET' && parts.length === 1 && parts[0] === 'app.js') {
        text(res, 200, await readFile(join(PUBLIC, 'app.js'), 'utf8'), 'text/javascript; charset=utf-8'); return;
      }
      if (req.method === 'GET' && parts.length === 1 && parts[0] === 'style.css') {
        text(res, 200, await readFile(join(PUBLIC, 'style.css'), 'utf8'), 'text/css; charset=utf-8'); return;
      }
      if (req.method === 'GET' && parts.length === 1 && parts[0] === 'favicon.ico') {
        res.writeHead(204, { 'cache-control': 'no-store' }); res.end(); return;
      }
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'status' && req.method === 'GET') {
        json(res, 200, tunnel.status()); return;
      }
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'start' && req.method === 'POST') {
        json(res, 200, await tunnel.start()); return;
      }
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'stop' && req.method === 'POST') {
        json(res, 200, await tunnel.stop()); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'overview') {
        // Degraded joins: one failing section must not blank the whole
        // console. Each fallback keeps the response shape so the UI's
        // existing `?? 'unavailable'` rendering stays honest.
        const [healthR, statusR, backupsR, namesR, keysR] = await Promise.allSettled([api.health(), api.status(), api.backups(), api.list(), api.keys()]);
        const health = healthR.status === 'fulfilled' ? healthR.value : { status: 'unavailable' };
        const status: Record<string, unknown> = statusR.status === 'fulfilled' ? statusR.value as unknown as Record<string, unknown> : {};
        // The discriminator matters: an empty per_database with no marker
        // renders as "clean", the one reading an unreachable backup root
        // must never produce.
        const backups = backupsR.status === 'fulfilled' ? backupsR.value : { backup_root: null, stale_after_secs: 0, sets: [], per_database: {}, unavailable: true };
        const names = namesR.status === 'fulfilled' ? namesR.value : [];
        const keys = keysR.status === 'fulfilled' ? keysR.value : [];
        const value = {
          observed_at: new Date().toISOString(),
          server_url: config.gatewayUrl,
          gateway: { available: health.status === 'ok', ...status },
          backups,
          databases: names,
          keys,
          resource_signals: [
            { name: 'rss', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote process RSS.' },
            { name: 'disk', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote filesystem capacity.' },
            { name: 'cpu', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote CPU utilization.' },
            { name: 'remote_host', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote host identity or reachability.' },
            { name: 'alerts', state: 'unavailable', source: 'local companion', reason: 'No alert collector is configured for this local console.' },
            { name: 'off_host_backup', state: 'not_configured', source: 'gateway', reason: 'No OVH/Litestream collector is configured in this local console.' },
            { name: 'restore_verification', state: 'unavailable', source: 'gateway', reason: 'A restore rehearsal is a separate operator action, not a live health signal.' },
          ],
          tunnel: tunnel.status(),
        };
        json(res, 200, value); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'databases' && parts[2]) {
        const [infoR, schemaR] = await Promise.allSettled([api.info(parts[2]), api.schema(parts[2])]);
        if (infoR.status !== 'fulfilled') throw infoR.reason;
        // Schema is advisory; the detail view renders the rest without it.
        json(res, 200, { ...infoR.value, schema: schemaR.status === 'fulfilled' ? schemaR.value : null }); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'query') {
        const input = await body(req) as { sql?: unknown; args?: unknown };
        if (typeof input.sql !== 'string' || (input.args !== undefined && !Array.isArray(input.args))) throw new ApiError('invalid_request', 'sql and args are required.', 400);
        const idempotencyKey = req.headers['idempotency-key'];
        if (idempotencyKey !== undefined && (typeof idempotencyKey !== 'string' || idempotencyKey.length > 128 || !/^[\x21-\x7e]+$/.test(idempotencyKey))) throw new ApiError('invalid_request', 'Invalid Idempotency-Key.', 400);
        json(res, 200, await api.query(parts[2], input.sql, input.args as never[], idempotencyKey ? { idempotencyKey } : {})); return;
      }
      if (req.method === 'GET' && parts.length === 2 && parts[0] === 'api' && parts[1] === 'databases') {
        const [namesR, statusR, backupsR] = await Promise.allSettled([api.list(), api.status(), api.backups()]);
        // The name list is essential: an empty list on failure would lie.
        if (namesR.status !== 'fulfilled') throw namesR.reason;
        // Usage is only unknown when the status fetch itself failed; a
        // database absent from a good status response has true zero
        // activity (the gateway records an entry per executed statement).
        const usage = statusR.status === 'fulfilled' ? statusR.value.databases ?? {} : null;
        const perDb = backupsR.status === 'fulfilled' ? backupsR.value.per_database : {};
        const entries = await Promise.all(namesR.value.map(async (name) => {
          let info;
          try {
            info = await api.info(name);
          } catch {
            info = null;
          }
          if (!info) {
            return { name, unknown: true as const, size_bytes: null, rows_read: null, rows_written: null, backup: { state: 'unknown' as const, newest_set: null, age_secs: null }, tombstoned: false as const, tables: [] };
          }
          const fresh = perDb[name] ?? { state: 'unknown' as const, newest_set: null, age_secs: null };
          const use = usage?.[name] ?? (usage === null ? { rows_read: null, rows_written: null } : { rows_read: 0, rows_written: 0 });
          const base = { name, size_bytes: info.size_bytes, rows_read: use.rows_read, rows_written: use.rows_written, backup: fresh };
          return info.tombstoned
            ? { ...base, tombstoned: true as const, tables: [] }
            : { ...base, tombstoned: false as const, created_at: info.created_at, tables: info.tables };
        }));
        json(res, 200, entries); return;
      }
      if (req.method === 'POST' && parts.length === 2 && parts[0] === 'api' && parts[1] === 'databases') {
        const input = await body(req) as { name?: unknown };
        if (typeof input.name !== 'string') throw new ApiError('invalid_request', 'name is required.', 400);
        json(res, 200, await api.create(input.name)); return;
      }
      if (req.method === 'DELETE' && parts[0] === 'api' && parts[1] === 'databases' && parts[2]) {
        json(res, 200, await api.deleteDatabase(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'batch') {
        const input = await body(req) as { statements?: unknown };
        if (!Array.isArray(input.statements)) throw new ApiError('invalid_request', 'statements are required.', 400);
        const idempotencyKey = req.headers['idempotency-key'];
        if (idempotencyKey !== undefined && (typeof idempotencyKey !== 'string' || idempotencyKey.length > 128 || !/^[\x21-\x7e]+$/.test(idempotencyKey))) throw new ApiError('invalid_request', 'Invalid Idempotency-Key.', 400);
        json(res, 200, await api.batch(parts[2], input.statements as never[], idempotencyKey ? { idempotencyKey } : {})); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'clone') {
        const input = await body(req) as { destination?: unknown };
        if (typeof input.destination !== 'string') throw new ApiError('invalid_request', 'destination is required.', 400);
        json(res, 200, await api.clone(parts[2], input.destination)); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'export') {
        json(res, 200, await api.exportDatabase(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'export-begin') {
        json(res, 200, await api.exportBegin(parts[2])); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'snapshots' && parts[2] && parts[3] === 'chunk') {
        const query = new URL(req.url ?? '/', 'http://console.local').searchParams;
        const offset = Number(query.get('offset') ?? 0);
        const length = Number(query.get('length') ?? 1024 * 1024);
        if (!Number.isInteger(offset) || !Number.isInteger(length) || offset < 0 || length < 1 || length > 1024 * 1024) throw new ApiError('invalid_request', 'Invalid snapshot range.', 400);
        const chunk = await api.exportChunk(parts[2], offset, length);
        json(res, 200, chunk); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'import') {
        const input = await body(req) as { name?: unknown; data_base64?: unknown; from?: unknown };
        if (typeof input.name !== 'string' || typeof input.data_base64 !== 'string') throw new ApiError('invalid_request', 'name and data_base64 are required.', 400);
        json(res, 200, await api.importDatabase(input.name, input.data_base64, typeof input.from === 'string' ? input.from : 'console')); return;
      }
      if (req.method === 'DELETE' && parts[0] === 'api' && parts[1] === 'snapshots' && parts[2]) {
        await api.releaseSnapshot(parts[2]); json(res, 200, { released: true }); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'uploads' && parts[2]) {
        json(res, 200, await api.uploadStatus(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'import-begin') {
        json(res, 200, await api.importBegin(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'uploads' && parts[2] && parts[3] === 'chunk') {
        const input = await body(req, MAX_CHUNK_BODY) as { offset?: unknown; data_base64?: unknown };
        if (typeof input.offset !== 'number' || typeof input.data_base64 !== 'string') throw new ApiError('invalid_request', 'offset and data_base64 are required.', 400);
        json(res, 200, await api.importChunk(parts[2], input.offset, input.data_base64)); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'uploads' && parts[2] && parts[3] === 'commit') {
        const input = await body(req) as { name?: unknown };
        if (typeof input.name !== 'string') throw new ApiError('invalid_request', 'name is required.', 400);
        json(res, 200, await api.importCommit(parts[2], input.name)); return;
      }
      if (req.method === 'DELETE' && parts[0] === 'api' && parts[1] === 'uploads' && parts[2]) {
        await api.abortUpload(parts[2]); json(res, 200, { aborted: true }); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'health') { json(res, 200, await api.health()); return; }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'status') { json(res, 200, await api.status()); return; }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'backups') { json(res, 200, await api.backups()); return; }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'keys') { json(res, 200, { keys: await api.keys() }); return; }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'keys') {
        const input = await body(req) as { name?: unknown; role?: unknown; databases?: unknown };
        if (typeof input.name !== 'string' || (input.role !== 'database_read_only' && input.role !== 'database_read_write') || !Array.isArray(input.databases) || !input.databases.every((x) => typeof x === 'string')) throw new ApiError('invalid_request', 'name, role, and databases are required.', 400);
        const key = await api.issueKey(input.name, input.role, input.databases);
        json(res, 200, { key, warning: 'This credential is shown once; store it outside the browser.' }); return;
      }
      if (req.method === 'DELETE' && parts[0] === 'api' && parts[1] === 'keys' && parts[2]) { await api.revokeKey(parts[2]); json(res, 200, { revoked: true }); return; }
      json(res, 404, { error: { code: 'not_found', message: 'Console route not found' } });
    } catch (error) {
      if (error instanceof ApiError) json(res, error.status, { error: { code: error.code, message: error.message } });
      else {
        logRequestFailure(error, credential);
        json(res, 500, { error: { code: 'console_error', message: 'Console request failed' } });
      }
    }
  });
  await new Promise<void>((resolve, reject) => { server.once('error', reject); server.listen(config.port, config.host, resolve); })
    .catch(async (error) => {
      // The listen was rejected (EADDRINUSE most commonly: a stale console,
      // a second `dg web`, or any unrelated listener on the configured
      // port). The ssh tunnel was started before listen; without this
      // unwind the tunnel child outlives the exited console and keeps
      // its loopback forward bound. Stop the tunnel and surface the
      // original listen error.
      if (config.tunnel) await tunnel.stop();
      throw error;
    });
  return {
    server,
    close: () => new Promise<void>((resolve) => {
      void tunnel.stop().finally(() => {
        server.close(() => resolve());
        server.closeAllConnections?.();
      });
    }),
  };
}

export function sessionId(): string { return randomBytes(18).toString('base64url'); }

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const config = configFromEnv();
  serve(config).then(({ close }) => {
    console.log(`DB Gateway console listening on http://${config.host}:${config.port}`);
    const stop = () => { void close().finally(() => process.exit(0)); };
    process.once('SIGINT', stop); process.once('SIGTERM', stop);
  }).catch((error) => { console.error('console failed to start:', error instanceof Error ? error.message : error); process.exit(1); });
}
