import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { createHash, randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { ApiError, GatewayApi } from './api.js';
import { TunnelController, type TunnelOptions } from './tunnel.js';

const ROOT = dirname(fileURLToPath(import.meta.url));
const PUBLIC = join(ROOT, '..', 'public');
const MAX_BODY = 12 * 1024 * 1024;
const LOOPBACK = new Set(['127.0.0.1', '::1', 'localhost']);

function env(name: string, fallback = ''): string { return process.env[name] ?? fallback; }
function isLoopback(host: string): boolean { return LOOPBACK.has(host); }
function json(res: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(value);
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
  res.end(body);
}
function text(res: ServerResponse, status: number, body: string, type = 'text/plain; charset=utf-8'): void {
  res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
  res.end(body);
}
async function body(req: IncomingMessage): Promise<unknown> {
  let size = 0; const chunks: Buffer[] = [];
  for await (const chunk of req) {
    const b = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    size += b.length;
    if (size > MAX_BODY) throw new ApiError('request_too_large', 'Request body is too large.', 413);
    chunks.push(b);
  }
  if (!chunks.length) return {};
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8')); } catch { throw new ApiError('invalid_request', 'Request body must be valid JSON.', 400); }
}
function pathParts(url: string): string[] { return new URL(url, 'http://console.local').pathname.split('/').filter(Boolean).map((x) => decodeURIComponent(x)); }
function trustedOrigin(origin: string | undefined, config: ConsoleConfig): boolean {
  if (!origin) return true;
  try {
    const url = new URL(origin);
    return url.protocol === 'http:' && LOOPBACK.has(url.hostname) && (config.port === 0 || Number(url.port || 80) === config.port);
  } catch { return false; }
}
function noSecret(value: unknown): void {
  const text = JSON.stringify(value);
  if (typeof text === 'string' && /[a-f0-9]{64}/i.test(text)) throw new Error('refusing to serialize a credential-shaped value');
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
  const tunnel: TunnelOptions | undefined = target ? {
    target,
    localPort,
    remoteHost: env('DG_SSH_REMOTE_HOST', '127.0.0.1'),
    remotePort: Number(env('DG_SSH_REMOTE_PORT', '7432')),
    identityFile: env('DG_SSH_IDENTITY_FILE') || undefined,
    command: env('DG_SSH_COMMAND') || undefined,
  } : undefined;
  const gatewayUrl = env('DG_SERVER', tunnel ? `http://127.0.0.1:${localPort}` : 'http://127.0.0.1:7432');
  const keyFile = env('DG_KEY_FILE', join(process.env.HOME ?? '', '.config/dg/key'));
  if (!keyFile) throw new Error('DG_KEY_FILE is required');
  return { host, port, gatewayUrl, keyFile, tunnel };
}

export async function readCredential(path: string): Promise<string> {
  const metadata = await stat(path);
  if (!metadata.isFile() || metadata.size > 4096) throw new Error('credential file must be a small regular file');
  if (process.platform !== 'win32' && (metadata.mode & 0o077) !== 0) throw new Error('credential file must not be accessible by group or others');
  const value = (await readFile(path, 'utf8')).trim();
  if (!/^[a-f0-9]{64}$/i.test(value)) throw new Error('credential file must contain one 64-character hex key');
  return value;
}

export async function serve(config: ConsoleConfig): Promise<{ close: () => Promise<void>; server: ReturnType<typeof createServer> }> {
  const credential = await readCredential(config.keyFile);
  const api = new GatewayApi(config.gatewayUrl, credential);
  const session = sessionId();
  const tunnel = new TunnelController(config.tunnel);
  if (config.tunnel) tunnel.start();
  const server = createServer(async (req, res) => {
    const parts = pathParts(req.url ?? '/');
    res.setHeader('x-content-type-options', 'nosniff');
    res.setHeader('x-frame-options', 'DENY');
    res.setHeader('referrer-policy', 'no-referrer');
    res.setHeader('content-security-policy', "default-src 'self'; connect-src 'self'; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'");
    try {
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
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'status' && req.method === 'GET') {
        json(res, 200, tunnel.status()); return;
      }
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'start' && req.method === 'POST') {
        json(res, 200, tunnel.start()); return;
      }
      if (parts[0] === 'api' && parts[1] === 'tunnel' && parts[2] === 'stop' && req.method === 'POST') {
        json(res, 200, tunnel.stop()); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'overview') {
        const [health, status, backups, names, keys] = await Promise.all([api.health(), api.status(), api.backups(), api.list(), api.keys()]);
        const value = {
          observed_at: new Date().toISOString(),
          gateway: { available: health.status === 'ok', ...status },
          backups,
          databases: names,
          keys,
          resource_signals: [
            { name: 'rss', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote process RSS.' },
            { name: 'disk', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote filesystem capacity.' },
            { name: 'cpu', state: 'unavailable', source: 'local companion', reason: 'The gateway API does not expose remote CPU utilization.' },
            { name: 'off_host_backup', state: 'not_configured', source: 'gateway', reason: 'No OVH/Litestream collector is configured in this local console.' },
            { name: 'restore_verification', state: 'unavailable', source: 'gateway', reason: 'A restore rehearsal is a separate operator action, not a live health signal.' },
          ],
          tunnel: tunnel.status(),
        };
        noSecret(value); json(res, 200, value); return;
      }
      if (req.method === 'GET' && parts[0] === 'api' && parts[1] === 'databases' && parts[2]) {
        const [info, schema] = await Promise.all([api.info(parts[2]), api.schema(parts[2])]);
        json(res, 200, { ...info, schema }); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'query') {
        const input = await body(req) as { sql?: unknown; args?: unknown };
        if (typeof input.sql !== 'string' || (input.args !== undefined && !Array.isArray(input.args))) throw new ApiError('invalid_request', 'sql and args are required.', 400);
        json(res, 200, await api.query(parts[2], input.sql, input.args as never[])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases') {
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
        json(res, 200, await api.batch(parts[2], input.statements as never[])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'schema') {
        json(res, 200, await api.schema(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'clone') {
        const input = await body(req) as { destination?: unknown };
        if (typeof input.destination !== 'string') throw new ApiError('invalid_request', 'destination is required.', 400);
        json(res, 200, await api.clone(parts[2], input.destination)); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'export') {
        json(res, 200, await api.exportDatabase(parts[2])); return;
      }
      if (req.method === 'POST' && parts[0] === 'api' && parts[1] === 'databases' && parts[2] && parts[3] === 'import') {
        const input = await body(req) as { name?: unknown; data_base64?: unknown; from?: unknown };
        if (typeof input.name !== 'string' || typeof input.data_base64 !== 'string') throw new ApiError('invalid_request', 'name and data_base64 are required.', 400);
        json(res, 200, await api.importDatabase(input.name, input.data_base64, typeof input.from === 'string' ? input.from : 'console')); return;
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
      else { console.error('console request failed', error instanceof Error ? error.message : 'unknown error'); json(res, 500, { error: { code: 'console_error', message: 'Console request failed' } }); }
    }
  });
  await new Promise<void>((resolve, reject) => { server.once('error', reject); server.listen(config.port, config.host, resolve); });
  return { server, close: () => new Promise<void>((resolve) => { tunnel.stop(); server.close(() => resolve()); }) };
}

export function sessionId(): string { return randomBytes(18).toString('base64url'); }
export function fingerprint(value: string): string { return createHash('sha256').update(value).digest('hex').slice(0, 12); }

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const config = configFromEnv();
  serve(config).then(({ close }) => {
    console.log(`DB Gateway console listening on http://${config.host}:${config.port}`);
    const stop = () => { void close().finally(() => process.exit(0)); };
    process.once('SIGINT', stop); process.once('SIGTERM', stop);
  }).catch((error) => { console.error(error instanceof Error ? error.message : error); process.exit(1); });
}
