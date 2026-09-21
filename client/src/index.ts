// Matches the server's JSON encoding; objects (including integer wrappers) are unsupported.
export type SqlValue = null | boolean | number | string | number[];
export interface Statement { sql: string; args?: SqlValue[] }
export interface QueryResult { columns: string[]; rows: SqlValue[][]; rows_affected: number }
export interface BatchResult { results: QueryResult[] }
export interface RequestOptions { idempotencyKey?: string; maxResponseBytes?: number }
export interface KeyRequest { name: string; role: 'database_read_only' | 'database_read_write'; databases: string[] }
export interface KeyInfo { name: string; role: string; databases: string[] }
export interface TableInfo { name: string; rows: number }
export type DatabaseInfo =
  | { name: string; tombstoned: false; size_bytes: number; created_at: number; tables: TableInfo[] }
  | { name: string; tombstoned: true; purge_after: number; size_bytes: number };
export interface SnapshotData { size: number; sha256: string; data: Uint8Array }
export class GatewayError extends Error {
  constructor(public readonly code: string, message: string, public readonly status?: number) { super(message); }
}
const NAME_RE = /^[a-z0-9][a-z0-9_]{0,63}$/;
const HEX64_RE = /^[a-f0-9]{64}$/;
const DEFAULT_MAX_RESPONSE = 266240;
const CHUNK_BYTES = 1024 * 1024;
const object = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
function isValue(v: unknown): v is SqlValue {
  return v === null || typeof v === 'string' || typeof v === 'boolean' || (typeof v === 'number' && Number.isFinite(v)) ||
    (Array.isArray(v) && v.every(b => Number.isInteger(b) && b >= 0 && b <= 255));
}
function isResult(v: unknown): v is QueryResult {
  return object(v) && Array.isArray(v.columns) && v.columns.every(c => typeof c === 'string') &&
    Array.isArray(v.rows) && v.rows.every(row => Array.isArray(row) && row.length === (v.columns as unknown[]).length && row.every(isValue)) &&
    typeof v.rows_affected === 'number' && Number.isSafeInteger(v.rows_affected) && v.rows_affected >= 0;
}
const isStringArray = (v: unknown): v is string[] => Array.isArray(v) && v.every(s => typeof s === 'string');
const isHex64 = (v: unknown): v is string => typeof v === 'string' && HEX64_RE.test(v);
function assertName(name: string, what = 'database'): void {
  if (!NAME_RE.test(name)) throw new Error(`Invalid ${what} name`);
}
function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(binary);
}
function base64ToBytes(b64: string): Uint8Array {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}
async function sha256Hex(data: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', data as BufferSource);
  return [...new Uint8Array(digest)].map(b => b.toString(16).padStart(2, '0')).join('');
}
function isKeyInfo(v: unknown): v is KeyInfo {
  return object(v) && typeof v.name === 'string' && typeof v.role === 'string' && isStringArray(v.databases);
}
function isDatabaseInfo(v: unknown): v is DatabaseInfo {
  if (!object(v) || typeof v.name !== 'string' || typeof v.tombstoned !== 'boolean' ||
      typeof v.size_bytes !== 'number' || !Number.isSafeInteger(v.size_bytes)) return false;
  if (v.tombstoned) return typeof v.purge_after === 'number' && Number.isSafeInteger(v.purge_after);
  return typeof v.created_at === 'number' && Number.isSafeInteger(v.created_at) && Array.isArray(v.tables) &&
    v.tables.every(t => object(t) && typeof t.name === 'string' && typeof t.rows === 'number' && Number.isSafeInteger(t.rows));
}
export class Gateway {
  private readonly origin: string;
  constructor(server: string, private readonly credential: string) {
    const url = new URL(server);
    if (!(url.protocol === 'https:' || (url.protocol === 'http:' && (url.hostname === '127.0.0.1' || url.hostname === '[::1]'))) || url.username || url.password || url.search || url.hash || url.pathname !== '/') {
      throw new Error('Use HTTPS or literal loopback HTTP origin');
    }
    this.origin = url.origin;
  }
  database(name: string): Database {
    assertName(name);
    return new Database(this, name);
  }
  async request(path: string, method: string, body?: unknown, options: RequestOptions = {}): Promise<unknown> {
    const headers: Record<string,string> = { Authorization: `Bearer ${this.credential}`, 'Content-Type': 'application/json' };
    if (options.idempotencyKey !== undefined) headers['Idempotency-Key'] = options.idempotencyKey;
    const limit = options.maxResponseBytes ?? DEFAULT_MAX_RESPONSE;
    let response: Response;
    let raw: unknown;
    try {
      response = await fetch(this.origin + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), redirect: 'error', signal: AbortSignal.timeout(10_000) });
      const reader = response.body?.getReader();
      if (!reader) throw new Error('Missing response');
      const chunks: Uint8Array[] = [];
      let size = 0;
      try {
        for (;;) {
          const {value,done} = await reader.read();
          if (done) break;
          size += value.length;
          if (size > limit) throw new Error('Response limit');
          chunks.push(value);
        }
      } finally { await reader.cancel(); }
      const bytes = new Uint8Array(size); let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk,offset); offset += chunk.length; }
      raw = JSON.parse(new TextDecoder('utf-8', {fatal:true}).decode(bytes));
    } catch {
      throw new GatewayError('outcome_unknown','Transport/response failed. A write may have committed. Replay with the same key/body within retention; never blindly retry an unkeyed write.');
    }
    if (!response.ok) {
      if (object(raw) && object(raw.error) && typeof raw.error.code === 'string' && typeof raw.error.message === 'string') {
        throw new GatewayError(raw.error.code, raw.error.message, response.status);
      }
      throw new GatewayError('http_error','Invalid error response',response.status);
    }
    return raw;
  }
  async createDatabase(name: string): Promise<{ name: string }> {
    assertName(name);
    const result = await this.request('/v1/databases', 'POST', { name });
    if (!object(result) || result.name !== name) throw new GatewayError('outcome_unknown','Malformed create response');
    return { name };
  }
  async listDatabases(): Promise<string[]> {
    const result = await this.request('/v1/databases', 'GET');
    if (!object(result) || !isStringArray(result.databases)) throw new GatewayError('outcome_unknown','Malformed list response');
    return result.databases;
  }
  async databaseInfo(name: string): Promise<DatabaseInfo> {
    assertName(name);
    const result = await this.request(`/v1/databases/${name}`, 'GET');
    if (!isDatabaseInfo(result)) throw new GatewayError('outcome_unknown','Malformed info response');
    return result;
  }
  async deleteDatabase(name: string): Promise<{ deleted: boolean; purge_after_seconds: number }> {
    assertName(name);
    const result = await this.request(`/v1/databases/${name}`, 'DELETE');
    if (!object(result) || result.deleted !== true || typeof result.purge_after_seconds !== 'number') {
      throw new GatewayError('outcome_unknown','Malformed delete response');
    }
    return { deleted: true, purge_after_seconds: result.purge_after_seconds };
  }
  async issueKey(req: KeyRequest): Promise<{ key: string }> {
    assertName(req.name, 'credential');
    if ((req.role !== 'database_read_only' && req.role !== 'database_read_write') ||
        req.databases.length === 0 || req.databases.length > 32) {
      throw new Error('Invalid key request');
    }
    for (const db of req.databases) assertName(db);
    const result = await this.request('/v1/keys', 'POST', req);
    if (!object(result) || !isHex64(result.key)) throw new GatewayError('outcome_unknown','Malformed issue response');
    return { key: result.key };
  }
  async listKeys(): Promise<KeyInfo[]> {
    const result = await this.request('/v1/keys', 'GET');
    if (!object(result) || !Array.isArray(result.keys) || !result.keys.every(isKeyInfo)) {
      throw new GatewayError('outcome_unknown','Malformed key list response');
    }
    return result.keys;
  }
  async revokeKey(name: string): Promise<{ revoked: boolean }> {
    assertName(name, 'credential');
    const result = await this.request(`/v1/keys/${name}`, 'DELETE');
    if (!object(result) || result.revoked !== true) throw new GatewayError('outcome_unknown','Malformed revoke response');
    return { revoked: true };
  }
  async importSnapshot(name: string, data: Uint8Array): Promise<{ imported: string }> {
    assertName(name);
    const result = await this.request(`/v1/databases/${name}/import`, 'POST', { name, data_base64: bytesToBase64(data) });
    if (!object(result) || result.imported !== name) throw new GatewayError('outcome_unknown','Malformed import response');
    return { imported: name };
  }
  async importChunked(name: string, data: Uint8Array): Promise<{ imported: string }> {
    assertName(name);
    const begin = await this.request(`/v1/databases/${name}/import/begin`, 'POST', {});
    if (!object(begin) || !isHex64(begin.upload)) throw new GatewayError('outcome_unknown','Malformed upload begin response');
    const upload = begin.upload;
    for (let offset = 0; offset < data.length; offset += CHUNK_BYTES) {
      const part = data.subarray(offset, offset + CHUNK_BYTES);
      const ack = await this.request(`/v1/uploads/${upload}/chunk`, 'POST', { offset, data_base64: bytesToBase64(part) });
      if (!object(ack) || ack.received !== offset + part.length) {
        throw new GatewayError('outcome_unknown','Upload acknowledgement mismatch');
      }
    }
    const done = await this.request(`/v1/uploads/${upload}/commit`, 'POST', { name });
    if (!object(done) || done.imported !== name) throw new GatewayError('outcome_unknown','Malformed commit response');
    return { imported: name };
  }
}
export class Database {
  constructor(private readonly gateway: Gateway, private readonly name: string) {}
  async execute(sql: string, args: SqlValue[] = [], options: RequestOptions = {}): Promise<QueryResult> {
    const result = await this.gateway.request(`/v1/databases/${this.name}/query`, 'POST', {sql,args},options);
    if (!isResult(result)) throw new GatewayError('outcome_unknown','Malformed query response');
    return result;
  }
  async batch(statements: Statement[], options: RequestOptions = {}): Promise<BatchResult> {
    const result = await this.gateway.request(`/v1/databases/${this.name}/batch`,'POST',{statements,atomic:true},options);
    if (!object(result) || !Array.isArray(result.results) || !result.results.every(isResult)) throw new GatewayError('outcome_unknown','Malformed batch response');
    return {results: result.results};
  }
  // Deliberate caller-initiated replay. No hidden automatic retry policy.
  retry(sql: string, args: SqlValue[], idempotencyKey: string): Promise<QueryResult> {
    if (!idempotencyKey) throw new Error('Replay requires original idempotency key');
    return this.execute(sql,args,{idempotencyKey});
  }
  async cloneTo(destination: string): Promise<{ source: string; destination: string }> {
    assertName(destination);
    const result = await this.gateway.request(`/v1/databases/${this.name}/clone`, 'POST', { destination });
    if (!object(result) || result.source !== this.name || result.destination !== destination) {
      throw new GatewayError('outcome_unknown','Malformed clone response');
    }
    return { source: this.name, destination };
  }
  async exportSnapshot(): Promise<SnapshotData & { database: string }> {
    const result = await this.gateway.request(
      `/v1/databases/${this.name}/export`, 'POST', {}, { maxResponseBytes: 12 * 1024 * 1024 });
    if (!object(result) || result.database !== this.name || typeof result.sha256 !== 'string' ||
        typeof result.size !== 'number' || typeof result.data_base64 !== 'string') {
      throw new GatewayError('outcome_unknown','Malformed export response');
    }
    const data = base64ToBytes(result.data_base64);
    if (data.length !== result.size || await sha256Hex(data) !== result.sha256) {
      throw new GatewayError('outcome_unknown','Export integrity mismatch');
    }
    return { database: this.name, size: result.size, sha256: result.sha256, data };
  }
  async exportChunked(): Promise<SnapshotData> {
    const begin = await this.gateway.request(`/v1/databases/${this.name}/export/begin`, 'POST', {});
    if (!object(begin) || !isHex64(begin.snapshot) || typeof begin.size !== 'number' || typeof begin.sha256 !== 'string') {
      throw new GatewayError('outcome_unknown','Malformed export begin response');
    }
    const { snapshot, size, sha256 } = begin as { snapshot: string; size: number; sha256: string };
    const parts: Uint8Array[] = [];
    let offset = 0;
    while (offset < size) {
      const chunk = await this.gateway.request(
        `/v1/snapshots/${snapshot}?offset=${offset}&length=${CHUNK_BYTES}`, 'GET', undefined,
        { maxResponseBytes: 2 * 1024 * 1024 });
      if (!object(chunk) || typeof chunk.data_base64 !== 'string' || chunk.offset !== offset) {
        throw new GatewayError('outcome_unknown','Malformed chunk response');
      }
      const bytes = base64ToBytes(chunk.data_base64);
      if (bytes.length === 0) break;
      parts.push(bytes);
      offset += bytes.length;
    }
    const data = new Uint8Array(offset);
    let at = 0;
    for (const part of parts) { data.set(part, at); at += part.length; }
    if (offset !== size || await sha256Hex(data) !== sha256) {
      throw new GatewayError('outcome_unknown','Export integrity mismatch');
    }
    return { size, sha256, data };
  }
}
