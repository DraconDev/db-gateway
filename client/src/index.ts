export type SqlValue = null | boolean | number | string | number[] | { integer: string };
export interface Statement { sql: string; args?: SqlValue[] }
export interface QueryResult { columns: string[]; rows: SqlValue[][]; rows_affected: number }
export interface BatchResult { results: QueryResult[] }
export interface RequestOptions { idempotencyKey?: string }
export class GatewayError extends Error {
  constructor(public readonly code: string, message: string, public readonly status?: number) { super(message); }
}
const object = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);
function isValue(v: unknown): v is SqlValue {
  return v === null || typeof v === 'string' || typeof v === 'boolean' || (typeof v === 'number' && Number.isFinite(v)) ||
    (Array.isArray(v) && v.every(b => Number.isInteger(b) && b >= 0 && b <= 255)) ||
    (object(v) && Object.keys(v).length === 1 && typeof v.integer === 'string' && /^-?\d+$/.test(v.integer));
}
function isResult(v: unknown): v is QueryResult {
  return object(v) && Array.isArray(v.columns) && v.columns.every(c => typeof c === 'string') &&
    Array.isArray(v.rows) && v.rows.every(row => Array.isArray(row) && row.length === (v.columns as unknown[]).length && row.every(isValue)) &&
    typeof v.rows_affected === 'number' && Number.isSafeInteger(v.rows_affected) && v.rows_affected >= 0;
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
    if (!/^[a-z0-9][a-z0-9_]{0,63}$/.test(name)) throw new Error('Invalid database name');
    return new Database(this, name);
  }
  async request(path: string, method: string, body?: unknown, options: RequestOptions = {}): Promise<unknown> {
    const headers: Record<string,string> = { Authorization: `Bearer ${this.credential}`, 'Content-Type': 'application/json' };
    if (options.idempotencyKey !== undefined) headers['Idempotency-Key'] = options.idempotencyKey;
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
          if (size > 266240) throw new Error('Response limit');
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
}
