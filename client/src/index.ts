// Matches the server's JSON encoding; objects (including integer wrappers) are unsupported.
export type SqlValue = null | boolean | number | string | number[];
export interface Statement { sql: string; args?: SqlValue[] }
export interface QueryResult { columns: string[]; rows: SqlValue[][]; rows_affected: number }
export interface BatchResult { results: QueryResult[] }
/// Whether a request can have committed data. The HTTP method cannot
/// answer this — a query is a POST whether it reads or writes — so the
/// statements decide, with the same rule the server uses.
export type Semantics = 'read' | 'write';
export interface RequestOptions {
  idempotencyKey?: string;
  maxResponseBytes?: number;
  /// What this request can commit. Defaults to the method's own answer
  /// (GET reads, everything else writes), which is correct for the
  /// management routes and fail-closed for anything else. Query and
  /// batch pass the statement-derived answer explicitly.
  semantics?: Semantics;
}
export interface KeyRequest { name: string; role: 'database_read_only' | 'database_read_write'; databases: string[] }
export interface KeyInfo { name: string; role: string; databases: string[] }
export interface TableInfo { name: string; rows: number }
export interface DatabaseUsage {
  rows_read: number;
  rows_written: number;
  queue_waits: number;
  queue_wait_ms: number;
  queue_timeouts: number;
  /** The longest single admission wait, in microseconds. */
  queue_wait_max_us?: number;
  /** This database's own 95th percentile of its own recent admission waits, in microseconds. */
  queue_wait_p95_us?: number | null;
  /** When the database was last written, as unix seconds. `null` when it has never been written. */
  last_write?: number | null;
}
export interface StatusResponse {
  version: string;
  uptime_secs: number;
  admission: { acquire_timeout_ms: number; read_permits: number; write_permits: number };
  requests: {
    total: number;
    capacity_exceeded: number;
    query_deadline_exceeded: number;
    internal_error: number;
    /** The per-second request counts of the recent window, oldest first. */
    recent_per_second?: number[];
    /** How many seconds the window observed. */
    recent_span_secs?: number;
    /** The median and 95th percentile admission wait, in microseconds. */
    wait_p50_us?: number | null;
    wait_p95_us?: number | null;
    wait_samples?: number;
  };
  databases?: Record<string, DatabaseUsage>;
  /** How many databases are live, and how many are tombstoned. */
  database_counts?: { live: number; tombstoned: number };
}
export type DatabaseInfo =
  | { name: string; tombstoned: false; size_bytes: number; created_at: number; tables: TableInfo[] }
  | { name: string; tombstoned: true; purge_after: number; size_bytes: number };
export interface SnapshotData { size: number; sha256: string; data: Uint8Array }
export class GatewayError extends Error {
  constructor(public readonly code: string, message: string, public readonly status?: number) { super(message); }
}
const NAME_RE = /^[a-z0-9][a-z0-9_]{0,63}$/;
const HEX64_RE = /^[a-f0-9]{64}$/;
const MAX_TRANSFER_BYTES = 128 * 1024 * 1024;
const DEFAULT_MAX_RESPONSE = 266240;
const CHUNK_BYTES = 1024 * 1024;

/// How long a read waits before the client gives up. A read that is given
/// up on costs a retry and nothing else, so it gives up quickly.
const READ_TIMEOUT_MS = 10_000;

/// How long a write waits before the client gives up. A write that is
/// given up on leaves the outcome in doubt, so it waits far longer: the
/// answer is what removes the doubt.
const WRITE_TIMEOUT_MS = 60_000;

/// Data-modifying keywords. A statement that opens like a read but holds
/// one of these is a write.
const WRITE_KEYWORDS = ['INSERT', 'UPDATE', 'DELETE', 'REPLACE', 'CREATE', 'DROP', 'ALTER', 'VACUUM', 'REINDEX'];

/// Whole-word scan for data-modifying keywords, so a WITH..SELECT whose
/// CTE is named `updated` stays on the read lane while WITH..INSERT does
/// not. False positives (a literal saying 'deleted') only cost the write
/// lane, never correctness.
function hasWriteKeyword(sql: string): boolean {
  const bytes = sql;
  let i = 0;
  while (i < bytes.length) {
    const c = bytes.charCodeAt(i);
    const alpha = (c >= 65 && c <= 90) || (c >= 97 && c <= 122);
    if (alpha) {
      const start = i;
      while (i < bytes.length) {
        const d = bytes.charCodeAt(i);
        const word = (d >= 65 && d <= 90) || (d >= 97 && d <= 122) || (d >= 48 && d <= 57) || d === 95;
        if (!word) break;
        i += 1;
      }
      if (WRITE_KEYWORDS.includes(bytes.slice(start, i))) return true;
    } else {
      i += 1;
    }
  }
  return false;
}

/// Whether a statement can only read.
///
/// The same rule the server uses for its admission lane (and the same one
/// `dg_client::is_read_only` uses): the statement opens with
/// SELECT/VALUES/EXPLAIN, or with WITH and holds no data-modifying
/// keyword. It fails closed — anything ambiguous takes the write lane,
/// which is slower for that request and never wrong.
export function isReadOnly(sql: string): boolean {
  const upper = sql.trimStart().toUpperCase();
  return (upper.startsWith('SELECT') || upper.startsWith('VALUES') || upper.startsWith('EXPLAIN') || upper.startsWith('WITH'))
    && !hasWriteKeyword(upper);
}

/// The transport semantics of one statement.
export function semanticsOf(sql: string): Semantics {
  return isReadOnly(sql) ? 'read' : 'write';
}

/// The transport semantics of a set of statements: a batch is a read only
/// when every statement in it is one.
export function semanticsOfStatements(statements: readonly Statement[]): Semantics {
  return statements.every(s => isReadOnly(s.sql)) ? 'read' : 'write';
}
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
const isTransferSize = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0 && v <= MAX_TRANSFER_BYTES;
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
function decodeExportBase64(encoded: unknown): Uint8Array {
  if (typeof encoded !== 'string') throw new GatewayError('outcome_unknown', 'Malformed export response');
  try {
    return base64ToBytes(encoded);
  } catch {
    throw new GatewayError('outcome_unknown', 'Malformed export response');
  }
}
function assertValues(args: SqlValue[]): void {
  if (!Array.isArray(args) || !args.every(isValue)) throw new Error('Invalid SQL argument');
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
const isNonNegativeInt = (v: unknown): v is number => typeof v === 'number' && Number.isSafeInteger(v) && v >= 0;
function isDatabaseUsage(v: unknown): v is DatabaseUsage {
  return object(v) && isNonNegativeInt(v.rows_read) && isNonNegativeInt(v.rows_written) &&
    isNonNegativeInt(v.queue_waits) && isNonNegativeInt(v.queue_wait_ms) && isNonNegativeInt(v.queue_timeouts);
}
function isStatusResponse(v: unknown): v is StatusResponse {
  if (!object(v) || typeof v.version !== 'string' || !isNonNegativeInt(v.uptime_secs) ||
      !object(v.admission) || !object(v.requests)) return false;
  if (v.databases !== undefined && (!object(v.databases) || !Object.values(v.databases).every(isDatabaseUsage))) return false;
  const admission = v.admission, requests = v.requests;
  return isNonNegativeInt(admission.acquire_timeout_ms) && isNonNegativeInt(admission.read_permits) &&
    isNonNegativeInt(admission.write_permits) && isNonNegativeInt(requests.total) &&
    isNonNegativeInt(requests.capacity_exceeded) && isNonNegativeInt(requests.query_deadline_exceeded) &&
    isNonNegativeInt(requests.internal_error);
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
    // What this request can commit: the statements decide when the caller
    // knows them, and the method is the answer otherwise (GET reads; the
    // management routes are all writes). A read cannot have committed
    // anything, so only a write leaves the outcome genuinely in doubt.
    // Collapsing the two told the operator a write may have committed even
    // for a failed list/status call -- and, in the other direction, turned
    // a read-only query into "outcome unknown" it could never have caused.
    const mayWrite = (options.semantics ?? (method === 'GET' ? 'read' : 'write')) === 'write';
    // A read that is given up on costs a retry and nothing else, so it gives
    // up quickly. A write is not the same: giving up on one leaves the
    // outcome in doubt, and the answer is what removes the doubt -- so a
    // write waits far longer. The CLI and the Rust client follow the same
    // rule, and the three encode one transport contract.
    const timeoutMs = mayWrite ? WRITE_TIMEOUT_MS : READ_TIMEOUT_MS;
    const readFailure = (detail: string) => new GatewayError('transport_unavailable', `${detail} The gateway is unavailable through the configured origin.`, 503);
    const writeInDoubt = (detail: string) => new GatewayError('outcome_unknown', `${detail} A write may have committed. Replay with the same key/body within retention; never blindly retry an unkeyed write.`);
    let response: Response;
    try {
      response = await fetch(this.origin + path, { method, headers, body: body === undefined ? undefined : JSON.stringify(body), redirect: 'error', signal: AbortSignal.timeout(timeoutMs) });
    } catch {
      if (!mayWrite) throw readFailure('The request was not answered before the read timeout.');
      throw writeInDoubt('Transport failed.');
    }
    let bytes: Uint8Array;
    try {
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
      bytes = new Uint8Array(size);
      let offset = 0;
      for (const chunk of chunks) { bytes.set(chunk,offset); offset += chunk.length; }
    } catch (error) {
      const detail = error instanceof Error && error.message === 'Response limit' ? 'The gateway response exceeded the configured limit' : 'The gateway response was interrupted';
      if (!mayWrite) throw new GatewayError('invalid_gateway_response', `${detail}.`, 502);
      throw writeInDoubt(`${detail}.`);
    }
    let raw: unknown;
    try {
      raw = JSON.parse(new TextDecoder('utf-8', {fatal:true}).decode(bytes));
    } catch {
      if (!mayWrite) throw new GatewayError('invalid_gateway_response', 'Gateway returned invalid JSON.', 502);
      throw writeInDoubt('The gateway returned an unreadable response.');
    }
    if (!response.ok) {
      if (object(raw) && object(raw.error) && typeof raw.error.code === 'string' && typeof raw.error.message === 'string') {
        throw new GatewayError(raw.error.code, raw.error.message, response.status);
      }
      if (!mayWrite) throw new GatewayError('invalid_gateway_response', 'Gateway returned a malformed error response.', 502);
      throw writeInDoubt('The gateway returned an unreadable error response.');
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
    if (!object(result) || !isStringArray(result.databases)) throw new GatewayError('invalid_gateway_response','Malformed list response', 502);
    return result.databases;
  }
  async status(): Promise<StatusResponse> {
    const result = await this.request('/v1/status', 'GET');
    if (!isStatusResponse(result)) throw new GatewayError('invalid_gateway_response','Malformed status response', 502);
    return result;
  }
  async databaseInfo(name: string): Promise<DatabaseInfo> {
    assertName(name);
    const result = await this.request(`/v1/databases/${name}`, 'GET');
    if (!isDatabaseInfo(result)) throw new GatewayError('invalid_gateway_response','Malformed info response', 502);
    return result;
  }
  async deleteDatabase(name: string): Promise<{ deleted: boolean; purge_after_seconds: number }> {
    assertName(name);
    const result = await this.request(`/v1/databases/${name}`, 'DELETE');
    if (!object(result) || result.deleted !== true || !isNonNegativeInt(result.purge_after_seconds)) {
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
      throw new GatewayError('invalid_gateway_response','Malformed key list response', 502);
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
    if (!(data instanceof Uint8Array) || data.length > MAX_TRANSFER_BYTES) throw new Error('Import data exceeds the 128 MiB transfer bound');
    const begin = await this.request(`/v1/databases/${name}/import/begin`, 'POST', {});
    if (!object(begin) || !isHex64(begin.upload)) throw new GatewayError('outcome_unknown','Malformed upload begin response');
    const upload = begin.upload;
    try {
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
    } catch (error) {
      try { await this.request(`/v1/uploads/${upload}`, 'DELETE'); } catch { /* best-effort slot release */ }
      throw error;
    }
  }
}
export class Database {
  constructor(private readonly gateway: Gateway, private readonly name: string) {}
  async execute(sql: string, args: SqlValue[] = [], options: RequestOptions = {}): Promise<QueryResult> {
    if (typeof sql !== 'string' || sql.length === 0) throw new Error('Invalid SQL argument');
    assertValues(args);
    // The HTTP method cannot tell this read from this write -- a query is a
    // POST either way -- so the statement decides, with the same rule the
    // server and the Rust client use. A read-only query is then answered as
    // a read: it gives up quickly and is never reported as a write that may
    // have committed.
    const semantics = options.semantics ?? semanticsOf(sql);
    const result = await this.gateway.request(`/v1/databases/${this.name}/query`, 'POST', {sql,args},{...options, semantics});
    if (!isResult(result)) throw new GatewayError(semantics === 'read' ? 'invalid_gateway_response' : 'outcome_unknown','Malformed query response');
    return result;
  }
  async batch(statements: Statement[], options: RequestOptions = {}): Promise<BatchResult> {
    if (!Array.isArray(statements) || !statements.every(s => object(s) && typeof (s as {sql?: unknown}).sql === 'string' && ((s as {args?: unknown}).args === undefined || (Array.isArray((s as {args?: unknown}).args) && ((s as {args?: unknown}).args as unknown[]).every(isValue))))) {
      throw new Error('Invalid SQL argument');
    }
    // A batch is a read only when every statement in it is one.
    const semantics = options.semantics ?? semanticsOfStatements(statements);
    const result = await this.gateway.request(`/v1/databases/${this.name}/batch`,'POST',{statements,atomic:true},{...options, semantics});
    if (!object(result) || !Array.isArray(result.results) || !result.results.every(isResult)) throw new GatewayError(semantics === 'read' ? 'invalid_gateway_response' : 'outcome_unknown','Malformed batch response');
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
    if (!object(result) || result.database !== this.name || !isHex64(result.sha256) ||
        !isTransferSize(result.size) || typeof result.data_base64 !== 'string') {
      throw new GatewayError('outcome_unknown','Malformed export response');
    }
    const data = decodeExportBase64(result.data_base64);
    if (data.length !== result.size || data.length > MAX_TRANSFER_BYTES || await sha256Hex(data) !== result.sha256) {
      throw new GatewayError('outcome_unknown','Export integrity mismatch');
    }
    return { database: this.name, size: result.size, sha256: result.sha256, data };
  }
  async exportChunked(): Promise<SnapshotData> {
    const begin = await this.gateway.request(`/v1/databases/${this.name}/export/begin`, 'POST', {});
    if (!object(begin) || !isHex64(begin.snapshot)) {
      throw new GatewayError('outcome_unknown','Malformed export begin response');
    }
    const { snapshot } = begin;
    try {
      if (!isTransferSize(begin.size) || !isHex64(begin.sha256)) {
        throw new GatewayError('outcome_unknown','Malformed export begin response');
      }
      const { size, sha256 } = begin;
      const parts: Uint8Array[] = [];
      let offset = 0;
      while (offset < size) {
        const chunk = await this.gateway.request(
          `/v1/snapshots/${snapshot}?offset=${offset}&length=${CHUNK_BYTES}`, 'GET', undefined,
          { maxResponseBytes: 2 * 1024 * 1024 });
        if (!object(chunk) || chunk.snapshot !== snapshot || !isTransferSize(chunk.size) || chunk.size !== size ||
            chunk.sha256 !== sha256 || typeof chunk.data_base64 !== 'string' || chunk.offset !== offset) {
          throw new GatewayError('outcome_unknown','Malformed chunk response');
        }
        const bytes = decodeExportBase64(chunk.data_base64);
        if (bytes.length === 0 || bytes.length > CHUNK_BYTES || offset + bytes.length > size) {
          throw new GatewayError('outcome_unknown','Malformed chunk response');
        }
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
    } finally {
      // A completed transfer still occupies one of the server's eight slots;
      // release it on every path so normal client use cannot self-throttle.
      try {
        await this.gateway.request(`/v1/snapshots/${snapshot}`, 'DELETE');
      } catch {
        // Preserve the export result/error; the server TTL remains the fallback.
      }
    }
  }
}
