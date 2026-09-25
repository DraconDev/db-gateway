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
export interface ImportResponse { imported: string; from: string }
export interface RequestOptions { idempotencyKey?: string }
export interface StatusResponse {
  version: string;
  uptime_secs: number;
  admission: { acquire_timeout_ms: number; read_permits: number; write_permits: number };
  requests: { total: number; capacity_exceeded: number; query_deadline_exceeded: number; internal_error: number };
}
export interface BackupResponse {
  backup_root: string | null;
  stale_after_secs: number;
  sets: Array<{ timestamp: string; databases: string[]; has_registry: boolean; coverage_start: number | null; coverage_end: number | null }>;
  per_database: Record<string, { state: 'fresh' | 'stale' | 'unknown'; newest_set: string | null; age_secs: number | null }>;
}

export class ApiError extends Error {
  constructor(public readonly code: string, message: string, public readonly status: number) { super(message); }
}

function assertName(name: string): void {
  if (!/^[a-z0-9][a-z0-9_]{0,63}$/.test(name)) throw new Error('Invalid database or key name');
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function parse<T>(value: unknown, predicate: (v: unknown) => boolean, code = 'outcome_unknown'): T {
  if (!predicate(value)) throw new ApiError(code, 'Malformed gateway response', 502);
  return value as T;
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
    try { text = await response.text(); } catch {
      if (method !== 'GET') throw new ApiError('outcome_unknown', 'The gateway response was interrupted after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
      throw new ApiError('invalid_gateway_response', 'Gateway response could not be read.', 502);
    }
    let value: unknown;
    try { value = text ? JSON.parse(text) : null; } catch {
      if (method !== 'GET') throw new ApiError('outcome_unknown', 'The gateway returned an invalid response after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
      throw new ApiError('invalid_gateway_response', 'Gateway returned invalid JSON.', 502);
    }
    if (!response.ok) {
      if (isObject(value) && isObject(value.error) && typeof value.error.code === 'string' && typeof value.error.message === 'string') {
        throw new ApiError(value.error.code, value.error.message, response.status);
      }
      if (method !== 'GET') throw new ApiError('outcome_unknown', 'The gateway returned an invalid response after a write may have been sent. Replay only an identical request with the same Idempotency-Key.', 503);
      throw new ApiError('http_error', 'Gateway returned an invalid error response.', response.status);
    }
    return value;
  }

  async health(): Promise<{ status: string }> {
    let response: Response;
    try {
      response = await fetch(new URL('/health/live', this.base), { redirect: 'error', signal: AbortSignal.timeout(5_000) });
    } catch {
      throw new ApiError('transport_unavailable', 'The gateway is unavailable through the configured loopback connection.', 503);
    }
    if (!response.ok) throw new ApiError('transport_unavailable', `Gateway liveness returned HTTP ${response.status}.`, 503);
    return parse(await response.json(), (v): v is { status: string } => isObject(v) && v.status === 'ok');
  }

  async status(): Promise<StatusResponse> {
    return parse<StatusResponse>(await this.request('/v1/status', 'GET'), (v): v is StatusResponse => isObject(v) && typeof v.version === 'string' && typeof v.uptime_secs === 'number' && isObject(v.requests));
  }

  async backups(): Promise<BackupResponse> {
    return parse<BackupResponse>(await this.request('/v1/backups', 'GET'), (v): v is BackupResponse => isObject(v) && Array.isArray(v.sets) && isObject(v.per_database));
  }

  async list(): Promise<string[]> {
    const value = parse<{ databases: string[] }>(await this.request('/v1/databases', 'GET'), (v): v is { databases: string[] } => isObject(v) && Array.isArray(v.databases) && v.databases.every((x) => typeof x === 'string'));
    return value.databases;
  }

  async info(name: string): Promise<DatabaseInfo> {
    assertName(name);
    const value = parse<DatabaseInfo>(await this.request(`/v1/databases/${encodeURIComponent(name)}`, 'GET'), (v): v is DatabaseInfo => isObject(v) && typeof v.name === 'string' && typeof v.tombstoned === 'boolean' && typeof v.size_bytes === 'number');
    return value;
  }

  async create(name: string): Promise<CreateResponse> {
    assertName(name);
    return parse<CreateResponse>(await this.request('/v1/databases', 'POST', { name }), (v): v is CreateResponse => isObject(v) && v.name === name);
  }

  async query(name: string, sql: string, args: QueryValue[] = [], options: RequestOptions = {}): Promise<QueryResult> {
    assertName(name); assertQueryArgs(args);
    return parse<QueryResult>(await this.request(`/v1/databases/${encodeURIComponent(name)}/query`, 'POST', { sql, args }, options), isQueryResult);
  }

  async schema(name: string): Promise<QueryResult> {
    assertName(name);
    return this.query(name, "SELECT name, sql FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))!='_dg_' ORDER BY name");
  }

  async batch(name: string, statements: Array<{ sql: string; args?: QueryValue[] }>, options: RequestOptions = {}): Promise<{ results: QueryResult[] }> {
    assertName(name); assertStatements(statements);
    return parse<{ results: QueryResult[] }>(await this.request(`/v1/databases/${encodeURIComponent(name)}/batch`, 'POST', { atomic: true, statements }, options), (v): v is { results: QueryResult[] } => isObject(v) && Array.isArray(v.results) && v.results.every(isQueryResult));
  }

  async keys(): Promise<KeyInfo[]> {
    const value = parse<{ keys: KeyInfo[] }>(await this.request('/v1/keys', 'GET'), (v): v is { keys: KeyInfo[] } => isObject(v) && Array.isArray(v.keys));
    return value.keys;
  }

  async issueKey(name: string, role: CredentialRole, databases: string[]): Promise<string> {
    assertName(name);
    const value = parse<KeySecret>(await this.request('/v1/keys', 'POST', { name, role, databases }), (v): v is KeySecret => isObject(v) && typeof v.key === 'string');
    return value.key;
  }

  async revokeKey(name: string): Promise<void> {
    assertName(name);
    await this.request(`/v1/keys/${encodeURIComponent(name)}`, 'DELETE');
  }

  async exportDatabase(name: string): Promise<ExportResponse> {
    assertName(name);
    return parse<ExportResponse>(await this.request(`/v1/databases/${encodeURIComponent(name)}/export`, 'POST', {}), (v): v is ExportResponse => isObject(v) && v.database === name && typeof v.size === 'number' && typeof v.sha256 === 'string' && typeof v.data_base64 === 'string');
  }

  async importDatabase(name: string, dataBase64: string, from = 'console'): Promise<ImportResponse> {
    assertName(name);
    return parse<ImportResponse>(await this.request(`/v1/databases/${encodeURIComponent(from)}/import`, 'POST', { name, data_base64: dataBase64 }), (v): v is ImportResponse => isObject(v) && v.imported === name);
  }

  async clone(source: string, destination: string): Promise<CloneResponse> {
    assertName(source); assertName(destination);
    return parse<CloneResponse>(await this.request(`/v1/databases/${encodeURIComponent(source)}/clone`, 'POST', { destination }), (v): v is CloneResponse => isObject(v) && v.source === source && v.destination === destination);
  }

  async deleteDatabase(name: string): Promise<DeleteResponse> {
    assertName(name);
    return parse<DeleteResponse>(await this.request(`/v1/databases/${encodeURIComponent(name)}`, 'DELETE'), (v): v is DeleteResponse => isObject(v) && v.deleted === true && typeof v.purge_after_seconds === 'number');
  }
}

function isQueryValue(value: unknown): value is QueryValue {
  return value === null || typeof value === 'string' || typeof value === 'boolean' || (typeof value === 'number' && Number.isFinite(value)) || (Array.isArray(value) && value.every((x) => Number.isInteger(x) && x >= 0 && x <= 255));
}
function assertQueryArgs(value: unknown): asserts value is QueryValue[] {
  if (!Array.isArray(value) || !value.every(isQueryValue)) throw new Error('Invalid SQL argument');
}
function assertStatements(value: unknown): asserts value is Array<{ sql: string; args?: QueryValue[] }> {
  if (!Array.isArray(value) || value.some((item) => !isObject(item) || typeof item.sql !== 'string' || (item.args !== undefined && (!Array.isArray(item.args) || !item.args.every(isQueryValue))))) throw new Error('Invalid SQL batch');
}
function isQueryResult(value: unknown): value is QueryResult {
  if (!isObject(value) || !Array.isArray(value.columns) || !value.columns.every((x) => typeof x === 'string') || !Array.isArray(value.rows) || typeof value.rows_affected !== 'number') return false;
  const columns = value.columns;
  return value.rows.every((row) => Array.isArray(row) && row.length === columns.length && row.every(isQueryValue));
}
