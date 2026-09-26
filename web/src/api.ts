export type CredentialRole = 'database_read_only' | 'database_read_write';
export interface KeyInfo { name: string; role: string; databases: string[] }
export interface TableInfo { name: string; rows: number }
export type DatabaseInfo =
  | { name: string; tombstoned: false; size_bytes: number; created_at: number; tables: TableInfo[] }
  | { name: string; tombstoned: true; purge_after: number; size_bytes: number };
export interface SqlValue { null?: never; boolean?: never; number?: never; string?: never }
export type QueryValue = null | boolean | number | string | number[];
export interface QueryResult { columns: string[]; rows: QueryValue[][]; rows_affected: number }
export interface CreateResponse { name: string }
export interface KeySecret { key: string }
export interface CloneResponse { source: string; destination: string }
export interface DeleteResponse { deleted: boolean; purge_after_seconds: number }
export interface ExportResponse { database: string; size: number; sha256: string; data_base64: string }
export interface ImportResponse { imported: string; from?: string }
export interface DatabaseUsage {
  rows_read: number;
  rows_written: number;
}
export interface StatusResponse {
  version: string;
  uptime_secs: number;
  admission: { acquire_timeout_ms: number; read_permits: number; write_permits: number };
  requests: { total: number; capacity_exceeded: number; query_deadline_exceeded: number; internal_error: number };
  databases?: Record<string, DatabaseUsage>;
}
export interface BackupResponse {
  backup_root: string | null;
  stale_after_secs: number;
  sets: Array<{ timestamp: string; databases: string[]; has_registry: boolean; coverage_start: number | null; coverage_end: number | null }>;
  per_database: Record<string, { state: 'fresh' | 'stale' | 'unknown'; newest_set: string | null; age_secs: number | null }>;
}
export interface RequestOptions { idempotencyKey?: string }
export interface SnapshotBegin { snapshot: string; size: number; sha256: string }
export interface SnapshotChunk { snapshot: string; size: number; sha256: string; offset: number; data_base64: string }
export interface UploadBegin { upload: string }
export interface UploadStatus { upload: string; received: number; size_cap: number }
export interface UploadChunk { upload: string; received: number }
export interface ChunkedExport { size: number; sha256: string; data: Uint8Array }
export interface ChunkedImport { imported: string }
export type TransferDirection = 'export' | 'import';
export interface TransferProgress {
  direction: TransferDirection;
  session: string;
  transferred: number;
  total: number;
}
export interface ChunkedExportOptions {
  /** Continue a previously retained snapshot after a transport failure. */
  resume?: { session: SnapshotBegin; offset: number; prefix: Uint8Array };
  onSession?: (session: SnapshotBegin) => void;
  onProgress?: (progress: TransferProgress) => void;
  signal?: AbortSignal;
  /** Snapshots are retained for resumable retry by default. */
  releaseOnError?: boolean;
}
export interface ChunkedImportOptions {
  /** Continue a previously retained upload; status is read before sending. */
  upload?: string;
  onSession?: (session: UploadBegin) => void;
  onProgress?: (progress: TransferProgress) => void;
  signal?: AbortSignal;
  /** Uploads are retained for resumable retry by default. */
  abortOnError?: boolean;
}

export const MAX_CHUNK_BYTES = 1024 * 1024;
export const MAX_TRANSFER_BYTES = 128 * 1024 * 1024;

export class ApiError extends Error {
  constructor(public readonly code: string, message: string, public readonly status: number) {
    super(message);
    this.name = 'ApiError';
  }
}

export class TransferError extends ApiError {
  readonly state: {
    direction: TransferDirection;
    session: SnapshotBegin | UploadBegin;
    transferred: number;
    data?: Uint8Array;
  };

  constructor(error: unknown, state: TransferError['state']) {
    const cause = error instanceof ApiError
      ? error
      : new ApiError('outcome_unknown', 'The gateway transfer failed after a write may have been sent.', 503);
    super(cause.code, cause.message, cause.status);
    this.name = 'TransferError';
    this.state = state;
  }
}

class ResponseLimitError extends Error {}
const MAX_RESPONSE_BYTES = 16 * 1024 * 1024;
const REDACTED = '[REDACTED]';

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function redactText(value: string, credential: string): string {
  if (!credential) return value;
  return value.replace(new RegExp(escapeRegExp(credential), 'gi'), REDACTED);
}

/** Remove the owner credential from every value that can cross the API boundary. */
export function redactCredential<T>(value: T, credential: string): T {
  if (!credential) return value;
  const seen = new WeakSet<object>();
  const visit = (current: unknown): unknown => {
    if (typeof current === 'string') return redactText(current, credential);
    if (current === null || typeof current !== 'object') return current;
    if (seen.has(current)) return REDACTED;
    seen.add(current);
    if (Array.isArray(current)) return current.map(visit);
    return Object.fromEntries(Object.entries(current).map(([key, child]) => [redactText(key, credential), visit(child)]));
  };
  return visit(value) as T;
}

async function readBounded(response: Response, limit = MAX_RESPONSE_BYTES): Promise<string> {
  const declared = Number(response.headers.get('content-length') ?? 0);
  if (Number.isFinite(declared) && declared > limit) throw new ResponseLimitError('Gateway response exceeds the console limit.');
  if (!response.body) {
    const text = await response.text();
    if (new TextEncoder().encode(text).byteLength > limit) throw new ResponseLimitError('Gateway response exceeds the console limit.');
    return text;
  }
  const reader = response.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      if (!value) continue;
      size += value.byteLength;
      if (size > limit) throw new ResponseLimitError('Gateway response exceeds the console limit.');
      chunks.push(value);
    }
  } finally {
    await reader.cancel().catch(() => undefined);
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
  return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
}

function invalidInput(message: string): ApiError {
  return new ApiError('invalid_request', message, 400);
}

function assertName(name: string): void {
  if (!/^[a-z0-9][a-z0-9_]{0,63}$/.test(name)) throw invalidInput('Invalid database or key name');
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

function isNonNegativeInteger(value: unknown): value is number {
  return isFiniteNumber(value) && Number.isInteger(value) && value >= 0;
}

function isTransferId(value: unknown): value is string {
  return typeof value === 'string' && /^[a-f0-9]{64}$/.test(value);
}

function isErrorEnvelope(value: unknown): value is { error: { code: string; message: string } } {
  return isObject(value) && isObject(value.error) && typeof value.error.code === 'string' && value.error.code.length > 0 && typeof value.error.message === 'string' && value.error.message.length > 0;
}

function parse<T>(value: unknown, predicate: (v: unknown) => boolean, code = 'invalid_gateway_response'): T {
  if (!predicate(value)) throw new ApiError(code, 'Malformed gateway response', 502);
  return value as T;
}

function isDatabaseUsage(value: unknown): value is DatabaseUsage {
  return isObject(value) && isNonNegativeInteger(value.rows_read) && isNonNegativeInteger(value.rows_written);
}
function isStatusResponse(value: unknown): value is StatusResponse {
  if (!isObject(value) || typeof value.version !== 'string' || !isNonNegativeInteger(value.uptime_secs) || !isObject(value.admission) || !isObject(value.requests)) return false;
  if (value.databases !== undefined && (!isObject(value.databases) || !Object.values(value.databases).every(isDatabaseUsage))) return false;
  return isNonNegativeInteger(value.admission.acquire_timeout_ms)
    && isNonNegativeInteger(value.admission.read_permits)
    && isNonNegativeInteger(value.admission.write_permits)
    && isNonNegativeInteger(value.requests.total)
    && isNonNegativeInteger(value.requests.capacity_exceeded)
    && isNonNegativeInteger(value.requests.query_deadline_exceeded)
    && isNonNegativeInteger(value.requests.internal_error);
}

function isBackupResponse(value: unknown): value is BackupResponse {
  if (!isObject(value) || !(value.backup_root === null || typeof value.backup_root === 'string') || !isNonNegativeInteger(value.stale_after_secs) || !Array.isArray(value.sets) || !isObject(value.per_database)) return false;
  const validSet = (set: unknown): boolean => isObject(set)
    && typeof set.timestamp === 'string'
    && Array.isArray(set.databases) && set.databases.every((name) => typeof name === 'string')
    && typeof set.has_registry === 'boolean'
    && (set.coverage_start === null || isNonNegativeInteger(set.coverage_start))
    && (set.coverage_end === null || isNonNegativeInteger(set.coverage_end));
  const validEntry = (entry: unknown): boolean => isObject(entry)
    && (entry.state === 'fresh' || entry.state === 'stale' || entry.state === 'unknown')
    && (entry.newest_set === null || typeof entry.newest_set === 'string')
    && (entry.age_secs === null || isNonNegativeInteger(entry.age_secs));
  return value.sets.every(validSet) && Object.values(value.per_database).every(validEntry);
}

function isKeyInfo(value: unknown): value is KeyInfo {
  return isObject(value) && typeof value.name === 'string' && value.name.length > 0 && typeof value.role === 'string' && Array.isArray(value.databases) && value.databases.every((name) => typeof name === 'string');
}

function isTableInfo(value: unknown): value is TableInfo {
  return isObject(value) && typeof value.name === 'string' && isNonNegativeInteger(value.rows);
}

function isDatabaseInfo(value: unknown): value is DatabaseInfo {
  if (!isObject(value) || typeof value.name !== 'string' || typeof value.tombstoned !== 'boolean' || !isNonNegativeInteger(value.size_bytes)) return false;
  if (value.tombstoned) return isNonNegativeInteger(value.purge_after);
  return isNonNegativeInteger(value.created_at) && Array.isArray(value.tables) && value.tables.every(isTableInfo);
}

function isSnapshotBegin(value: unknown): value is SnapshotBegin {
  return isObject(value) && isTransferId(value.snapshot) && isNonNegativeInteger(value.size) && typeof value.sha256 === 'string' && /^[a-f0-9]{64}$/.test(value.sha256);
}

function isSnapshotChunk(value: unknown): value is SnapshotChunk {
  return isObject(value) && isTransferId(value.snapshot) && isNonNegativeInteger(value.size) && typeof value.sha256 === 'string' && /^[a-f0-9]{64}$/.test(value.sha256) && isNonNegativeInteger(value.offset) && typeof value.data_base64 === 'string';
}

function isUploadBegin(value: unknown): value is UploadBegin {
  return isObject(value) && isTransferId(value.upload);
}

function isUploadStatus(value: unknown): value is UploadStatus {
  return isObject(value) && isTransferId(value.upload) && isNonNegativeInteger(value.received) && isNonNegativeInteger(value.size_cap);
}

function isUploadChunk(value: unknown): value is UploadChunk {
  return isObject(value) && isTransferId(value.upload) && isNonNegativeInteger(value.received);
}

export class GatewayApi {
  constructor(private readonly base: string, private readonly credential: string) {
    const url = new URL(base);
    if (url.protocol !== 'http:' || !['127.0.0.1', '[::1]', 'localhost'].includes(url.hostname) || url.pathname !== '/' || url.search || url.hash || url.username || url.password) {
      throw new Error('Console gateway must use a loopback HTTP origin');
    }
  }

  private async request(path: string, method: string, body?: unknown, options: RequestOptions = {}): Promise<unknown> {
    const headers: Record<string, string> = { Authorization: `Bearer ${this.credential}`, 'Content-Type': 'application/json' };
    if (options.idempotencyKey) headers['Idempotency-Key'] = options.idempotencyKey;
    let response: Response;
    try {
      response = await fetch(new URL(path, this.base), { method, headers, body: body === undefined ? undefined : JSON.stringify(body), redirect: 'error', signal: AbortSignal.timeout(15_000) });
    } catch {
      if (method !== 'GET') {
        throw new ApiError('outcome_unknown', 'The gateway connection failed after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
      }
      throw new ApiError('transport_unavailable', 'The gateway is unavailable through the configured loopback connection.', 503);
    }
    let text: string;
    try { text = await readBounded(response); } catch (error) {
      const detail = error instanceof ResponseLimitError ? 'The gateway response exceeded the console limit' : 'The gateway response was interrupted';
      if (method !== 'GET') throw new ApiError('outcome_unknown', `${detail} after a write may have been sent. Replay only an identical request with the same Idempotency-Key.`, 503);
      throw new ApiError('invalid_gateway_response', 'Gateway response could not be read.', 502);
    }
    let value: unknown;
    try { value = text ? JSON.parse(text) : null; } catch {
      if (method !== 'GET') throw new ApiError('outcome_unknown', 'The gateway returned an invalid response after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
      throw new ApiError('invalid_gateway_response', 'Gateway returned invalid JSON.', 502);
    }
    if (!response.ok) {
      if (isErrorEnvelope(value)) throw new ApiError(redactText(value.error.code, this.credential), redactText(value.error.message, this.credential), response.status);
      if (method === 'GET') throw new ApiError('invalid_gateway_response', 'Gateway returned a malformed error response.', 502);
      throw new ApiError('outcome_unknown', 'The gateway returned an invalid response after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
    }
    return redactCredential(value, this.credential);
  }

  async health(): Promise<{ status: string }> {
    let response: Response;
    try {
      response = await fetch(new URL('/health/live', this.base), { redirect: 'error', signal: AbortSignal.timeout(5_000) });
    } catch {
      throw new ApiError('transport_unavailable', 'The gateway is unavailable through the configured loopback connection.', 503);
    }
    if (!response.ok) throw new ApiError('transport_unavailable', `Gateway liveness returned HTTP ${response.status}.`, 503);
    let text: string;
    try { text = await readBounded(response, 64 * 1024); } catch { throw new ApiError('invalid_gateway_response', 'Gateway liveness response was invalid.', 502); }
    let value: unknown;
    try { value = text ? JSON.parse(text) : null; } catch { throw new ApiError('invalid_gateway_response', 'Gateway returned invalid JSON.', 502); }
    return redactCredential(parse(value, (v): v is { status: string } => isObject(v) && v.status === 'ok'), this.credential);
  }

  async status(): Promise<StatusResponse> {
    return parse<StatusResponse>(await this.request('/v1/status', 'GET'), isStatusResponse, 'invalid_gateway_response');
  }

  async backups(): Promise<BackupResponse> {
    return parse<BackupResponse>(await this.request('/v1/backups', 'GET'), isBackupResponse, 'invalid_gateway_response');
  }

  async list(): Promise<string[]> {
    const value = parse<{ databases: string[] }>(await this.request('/v1/databases', 'GET'), (v): v is { databases: string[] } => isObject(v) && Array.isArray(v.databases) && v.databases.every((x) => typeof x === 'string'), 'invalid_gateway_response');
    return value.databases;
  }

  async info(name: string): Promise<DatabaseInfo> {
    assertName(name);
    const value = parse<DatabaseInfo>(await this.request(`/v1/databases/${encodeURIComponent(name)}`, 'GET'), (v): v is DatabaseInfo => isDatabaseInfo(v) && v.name === name, 'invalid_gateway_response');
    return value;
  }

  async create(name: string): Promise<CreateResponse> {
    assertName(name);
    return parse<CreateResponse>(await this.request('/v1/databases', 'POST', { name }), (v): v is CreateResponse => isObject(v) && v.name === name, 'outcome_unknown');
  }

  async query(name: string, sql: string, args: QueryValue[] = [], options: RequestOptions = {}): Promise<QueryResult> {
    assertName(name); assertQueryArgs(args);
    return parse<QueryResult>(await this.request(`/v1/databases/${encodeURIComponent(name)}/query`, 'POST', { sql, args }, options), isQueryResult, 'outcome_unknown');
  }

  async schema(name: string): Promise<QueryResult> {
    assertName(name);
    return this.query(name, "SELECT name, sql FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))!='_dg_' ORDER BY name");
  }

  async batch(name: string, statements: Array<{ sql: string; args?: QueryValue[] }>, options: RequestOptions = {}): Promise<{ results: QueryResult[] }> {
    assertName(name); assertStatements(statements);
    return parse<{ results: QueryResult[] }>(await this.request(`/v1/databases/${encodeURIComponent(name)}/batch`, 'POST', { atomic: true, statements }, options), (v): v is { results: QueryResult[] } => isObject(v) && Array.isArray(v.results) && v.results.every(isQueryResult), 'outcome_unknown');
  }

  async keys(): Promise<KeyInfo[]> {
    const value = parse<{ keys: KeyInfo[] }>(await this.request('/v1/keys', 'GET'), (v): v is { keys: KeyInfo[] } => isObject(v) && Array.isArray(v.keys) && v.keys.every(isKeyInfo), 'invalid_gateway_response');
    return value.keys;
  }

  async issueKey(name: string, role: CredentialRole, databases: string[]): Promise<string> {
    assertName(name);
    const value = parse<KeySecret>(await this.request('/v1/keys', 'POST', { name, role, databases }), (v): v is KeySecret => isObject(v) && typeof v.key === 'string' && /^[a-f0-9]{64}$/.test(v.key), 'outcome_unknown');
    return value.key;
  }

  async revokeKey(name: string): Promise<void> {
    assertName(name);
    await this.request(`/v1/keys/${encodeURIComponent(name)}`, 'DELETE');
  }

  async exportDatabase(name: string): Promise<ExportResponse> {
    assertName(name);
    return parse<ExportResponse>(await this.request(`/v1/databases/${encodeURIComponent(name)}/export`, 'POST', {}), (v): v is ExportResponse => isObject(v) && v.database === name && isNonNegativeInteger(v.size) && typeof v.sha256 === 'string' && /^[a-f0-9]{64}$/.test(v.sha256) && typeof v.data_base64 === 'string', 'outcome_unknown');
  }

  async exportBegin(name: string): Promise<SnapshotBegin> {
    assertName(name);
    return parse<SnapshotBegin>(await this.request(`/v1/databases/${encodeURIComponent(name)}/export/begin`, 'POST', {}), isSnapshotBegin, 'outcome_unknown');
  }

  async exportChunk(snapshot: string, offset: number, length: number): Promise<SnapshotChunk> {
    if (!isTransferId(snapshot) || !Number.isInteger(offset) || offset < 0 || !Number.isInteger(length) || length < 1 || length > MAX_CHUNK_BYTES) throw new ApiError('invalid_request', 'Invalid snapshot range.', 400);
    const chunk = parse<SnapshotChunk>(await this.request(`/v1/snapshots/${encodeURIComponent(snapshot)}?offset=${offset}&length=${length}`, 'GET'), isSnapshotChunk, 'invalid_gateway_response');
    const bytes = decodeBase64(chunk.data_base64, 'invalid_gateway_response', 502);
    if (bytes.length < 1 || bytes.length > length) throw new ApiError('invalid_gateway_response', 'Gateway returned an oversized or empty snapshot chunk.', 502);
    return chunk;
  }

  async releaseSnapshot(snapshot: string): Promise<void> {
    if (!isTransferId(snapshot)) throw new ApiError('invalid_request', 'Invalid snapshot id.', 400);
    await this.request(`/v1/snapshots/${encodeURIComponent(snapshot)}`, 'DELETE');
  }

  async exportChunked(name: string, options: ChunkedExportOptions = {}): Promise<ChunkedExport> {
    assertName(name);
    const begin = options.resume?.session ?? await this.exportBegin(name);
    if (!isSnapshotBegin(begin)) throw new ApiError('invalid_request', 'Invalid export transfer session.', 400);
    let offset = options.resume?.offset ?? 0;
    const prefix = options.resume?.prefix ? new Uint8Array(options.resume.prefix) : new Uint8Array();
    if (!Number.isInteger(offset) || offset < 0 || offset !== prefix.length || offset > begin.size) throw new ApiError('invalid_request', 'Invalid export resume offset.', 400);
    const chunks: Uint8Array[] = prefix.length ? [prefix] : [];
    let released = false;
    const report = (transferred: number): void => options.onProgress?.({ direction: 'export', session: begin.snapshot, transferred, total: begin.size });
    try {
      options.onSession?.(begin);
      report(offset);
      while (offset < begin.size) {
        throwIfAborted(options.signal);
        const length = Math.min(MAX_CHUNK_BYTES, begin.size - offset);
        const chunk = await this.exportChunk(begin.snapshot, offset, length);
        if (chunk.size !== begin.size || chunk.sha256 !== begin.sha256 || chunk.offset !== offset) throw new ApiError('outcome_unknown', 'Snapshot metadata changed during transfer.', 502);
        const bytes = decodeBase64(chunk.data_base64, 'outcome_unknown', 502);
        if (!bytes.length || bytes.length > length || offset + bytes.length > begin.size) throw new ApiError('outcome_unknown', 'Malformed snapshot chunk.', 502);
        chunks.push(bytes);
        offset += bytes.length;
        report(offset);
      }
      const data = concatBytes(chunks, offset);
      if (await sha256Hex(data) !== begin.sha256) throw new ApiError('outcome_unknown', 'Export integrity mismatch.', 502);
      try { await this.releaseSnapshot(begin.snapshot); released = true; } catch { /* server TTL remains the fallback */ }
      return { size: begin.size, sha256: begin.sha256, data };
    } catch (error) {
      if (options.releaseOnError && !released) {
        try { await this.releaseSnapshot(begin.snapshot); released = true; } catch { /* server TTL remains the fallback */ }
      }
      if (error instanceof TransferError) throw error;
      const partial = concatBytes(chunks, offset);
      throw new TransferError(error, { direction: 'export', session: begin, transferred: offset, data: partial });
    }
  }

  async importDatabase(name: string, dataBase64: string, from = 'console'): Promise<ImportResponse> {
    assertName(name); assertName(from);
    const bytes = decodeBase64(dataBase64, 'invalid_request', 400);
    if (bytes.length > 8 * 1024 * 1024) throw new ApiError('result_limit_exceeded', 'Import exceeds the bounded single-shot size.', 413);
    return parse<ImportResponse>(await this.request(`/v1/databases/${encodeURIComponent(from)}/import`, 'POST', { name, data_base64: dataBase64 }), (v): v is ImportResponse => isObject(v) && v.imported === name && (v.from === undefined || typeof v.from === 'string'), 'outcome_unknown');
  }

  async importBegin(from = 'console'): Promise<UploadBegin> {
    assertName(from);
    return parse<UploadBegin>(await this.request(`/v1/databases/${encodeURIComponent(from)}/import/begin`, 'POST', {}), isUploadBegin, 'outcome_unknown');
  }

  async importChunk(upload: string, offset: number, dataBase64: string): Promise<UploadChunk> {
    if (!isTransferId(upload) || !Number.isInteger(offset) || offset < 0 || typeof dataBase64 !== 'string') throw new ApiError('invalid_request', 'Invalid upload chunk.', 400);
    const bytes = decodeBase64(dataBase64, 'invalid_request', 400);
    if (bytes.length < 1 || bytes.length > MAX_CHUNK_BYTES) throw new ApiError('invalid_request', 'Upload chunk must contain between 1 byte and 1 MiB.', 400);
    if (offset + bytes.length > MAX_TRANSFER_BYTES) throw new ApiError('result_limit_exceeded', 'Upload exceeds the chunked transfer ceiling.', 413);
    return parse<UploadChunk>(await this.request(`/v1/uploads/${encodeURIComponent(upload)}/chunk`, 'POST', { offset, data_base64: dataBase64 }), isUploadChunk, 'outcome_unknown');
  }

  async importCommit(upload: string, name: string): Promise<ImportResponse> {
    if (!isTransferId(upload)) throw new ApiError('invalid_request', 'Invalid upload id.', 400);
    assertName(name);
    return parse<ImportResponse>(await this.request(`/v1/uploads/${encodeURIComponent(upload)}/commit`, 'POST', { name }), (v): v is ImportResponse => isObject(v) && v.imported === name, 'outcome_unknown');
  }

  async abortUpload(upload: string): Promise<void> {
    if (!isTransferId(upload)) throw new ApiError('invalid_request', 'Invalid upload id.', 400);
    await this.request(`/v1/uploads/${encodeURIComponent(upload)}`, 'DELETE');
  }

  async uploadStatus(id: string): Promise<UploadStatus> {
    if (!isTransferId(id)) throw new ApiError('invalid_request', 'Invalid upload id.', 400);
    return parse<UploadStatus>(await this.request(`/v1/uploads/${encodeURIComponent(id)}`, 'GET'), (v): v is UploadStatus => isUploadStatus(v) && v.upload === id && v.received <= v.size_cap, 'invalid_gateway_response');
  }

  async importChunked(name: string, data: Uint8Array, from = 'console', options: ChunkedImportOptions = {}): Promise<ChunkedImport> {
    assertName(name); assertName(from);
    if (!(data instanceof Uint8Array)) throw new ApiError('invalid_request', 'Import data must be bytes.', 400);
    if (data.length > MAX_TRANSFER_BYTES) throw new ApiError('result_limit_exceeded', 'Import exceeds 128 MiB.', 413);
    const session: UploadBegin = options.upload ? { upload: options.upload } : await this.importBegin(from);
    let offset = 0;
    try {
      options.onSession?.(session);
      const status = await this.uploadStatus(session.upload);
      offset = status.received;
      if (offset > data.length) throw new ApiError('invalid_request', 'Upload has received more bytes than the selected file.', 409);
      const report = (transferred: number): void => options.onProgress?.({ direction: 'import', session: session.upload, transferred, total: data.length });
      report(offset);
      while (offset < data.length) {
        throwIfAborted(options.signal);
        const end = Math.min(offset + MAX_CHUNK_BYTES, data.length);
        const ack = await this.importChunk(session.upload, offset, bytesToBase64(data.subarray(offset, end)));
        if (ack.received !== end) throw new ApiError('outcome_unknown', 'Upload acknowledgement mismatch.', 502);
        offset = end;
        report(offset);
      }
      const done = await this.importCommit(session.upload, name);
      report(data.length);
      return { imported: done.imported };
    } catch (error) {
      if (options.abortOnError) {
        try { await this.abortUpload(session.upload); } catch { /* server TTL remains the fallback */ }
      }
      if (error instanceof TransferError) throw error;
      throw new TransferError(error, { direction: 'import', session, transferred: offset });
    }
  }

  async resumeChunkedImport(upload: string, name: string, data: Uint8Array, from = 'console', options: Omit<ChunkedImportOptions, 'upload'> = {}): Promise<ChunkedImport> {
    return this.importChunked(name, data, from, { ...options, upload });
  }

  async clone(source: string, destination: string): Promise<CloneResponse> {
    assertName(source); assertName(destination);
    return parse<CloneResponse>(await this.request(`/v1/databases/${encodeURIComponent(source)}/clone`, 'POST', { destination }), (v): v is CloneResponse => isObject(v) && v.source === source && v.destination === destination, 'outcome_unknown');
  }

  async deleteDatabase(name: string): Promise<DeleteResponse> {
    assertName(name);
    return parse<DeleteResponse>(await this.request(`/v1/databases/${encodeURIComponent(name)}`, 'DELETE'), (v): v is DeleteResponse => isObject(v) && v.deleted === true && isNonNegativeInteger(v.purge_after_seconds), 'outcome_unknown');
  }
}

function decodeBase64(encoded: string, code: string, status: number): Uint8Array {
  if (typeof encoded !== 'string' || encoded.length === 0) throw new ApiError(code, 'Invalid base64 payload.', status);
  try {
    const binary = atob(encoded);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    if (bytesToBase64(bytes) !== encoded) throw new Error('non-canonical base64');
    return bytes;
  } catch {
    throw new ApiError(code, 'Invalid base64 payload.', status);
  }
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = '';
  for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(binary);
}

function concatBytes(chunks: Uint8Array[], total: number): Uint8Array {
  const data = new Uint8Array(total);
  let cursor = 0;
  for (const chunk of chunks) { data.set(chunk, cursor); cursor += chunk.length; }
  return data;
}

async function sha256Hex(data: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', data as BufferSource);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, '0')).join('');
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw new ApiError('transfer_aborted', 'Transfer was paused; retain the session and resume it explicitly.', 499);
}

function isQueryValue(value: unknown): value is QueryValue {
  return value === null || typeof value === 'string' || typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value)) || (Array.isArray(value) && value.every((x) => Number.isInteger(x) && x >= 0 && x <= 255));
}
function assertQueryArgs(value: unknown): asserts value is QueryValue[] {
  if (!Array.isArray(value) || !value.every(isQueryValue)) throw invalidInput('Invalid SQL argument');
}
function assertStatements(value: unknown): asserts value is Array<{ sql: string; args?: QueryValue[] }> {
  if (!Array.isArray(value) || value.some((item) => !isObject(item) || typeof item.sql !== 'string' || (item.args !== undefined && (!Array.isArray(item.args) || !item.args.every(isQueryValue))))) throw invalidInput('Invalid SQL batch');
}
function isQueryResult(value: unknown): value is QueryResult {
  if (!isObject(value) || !Array.isArray(value.columns) || !value.columns.every((x) => typeof x === 'string') || !Array.isArray(value.rows) || !isNonNegativeInteger(value.rows_affected)) return false;
  const columns = value.columns;
  return value.rows.every((row) => Array.isArray(row) && row.length === columns.length && row.every(isQueryValue));
}
