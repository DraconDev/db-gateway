// Shadow mode: run an application on its current database (Turso/libSQL, or
// anything shaped like `@libsql/client`) while every write it makes is
// mirrored to a dg database, so dg is tested against real traffic without the
// application depending on it.
//
// The primary stays the source of truth. The wrapper calls it exactly as the
// application did and returns its answer unchanged; only after the primary
// has acknowledged a write is that write queued for dg. The queue applies the
// writes in the order the primary acknowledged them, in the background:
// nothing the application awaits ever waits on dg, and nothing dg does can
// make the application's call fail. What does not match is reported to
// `onDivergence` instead.
//
//   import { createClient } from '@libsql/client';
//   import { Gateway } from '@dracon/db-gateway';
//   import { withShadow, compareDatabases } from '@dracon/db-gateway/shadow';
//
//   const turso = createClient({ url, authToken });
//   const db = withShadow(turso, new Gateway(dgUrl, dgKey).database('app'), {
//     sampleReads: 0.05,                       // re-run 5% of reads on dg and compare
//     onDivergence: (d) => log.warn('dg shadow', d),
//   });
//   // ...the application uses `db` exactly as it used `turso`...
//
//   // nightly, or on demand:
//   const report = await compareDatabases(turso, dgDatabase);
//
// See docs/shadow.md for how to seed the dg copy and what the reports mean.

import { Database, GatewayError, isReadOnly, type QueryResult, type SqlValue, type Statement } from './index.js';

/** A value as `@libsql/client` hands them over: it also has bigint and binary. */
export type PrimaryValue = null | undefined | boolean | number | bigint | string | ArrayBuffer | Uint8Array | Date;
/** A statement as `@libsql/client` takes it. */
export type PrimaryStatement = string | { sql: string; args?: PrimaryValue[] | Record<string, PrimaryValue> };
/** The result shape `@libsql/client` returns: rows are array-like. */
export interface PrimaryResult {
  columns: string[];
  rows: ArrayLike<unknown>[];
  rowsAffected: number;
}
/** The part of a `@libsql/client` Client the wrapper needs. */
export interface PrimaryClient {
  execute(statement: PrimaryStatement): Promise<PrimaryResult>;
  batch(statements: PrimaryStatement[], mode?: string): Promise<PrimaryResult[]>;
}

export type DivergenceKind =
  /** The primary accepted a write that dg refused or failed on. */
  | 'shadow_error'
  /** A sampled read returned different rows on dg. */
  | 'read_mismatch'
  /** `rows_affected` differed for a mirrored write. */
  | 'rows_affected_mismatch'
  /** The statement cannot be mirrored as is (named parameters, an unencodable value). */
  | 'unsupported'
  /** The statement depends on time or randomness, so the copies may drift without either being wrong. */
  | 'nondeterministic';

export interface Divergence {
  kind: DivergenceKind;
  sql: string;
  detail: string;
}

export interface ShadowOptions {
  /** Fraction of reads (0..1) re-run on dg and compared. Default 0: writes only. */
  sampleReads?: number;
  /** Called for every divergence. Default: `console.warn`. Never awaited. */
  onDivergence?: (divergence: Divergence) => void;
  /** How many mirrored operations may wait in the queue; beyond it new ones are dropped and reported. Default 10 000. */
  maxQueue?: number;
}

export interface ShadowStats {
  mirroredWrites: number;
  comparedReads: number;
  divergences: number;
  dropped: number;
  queued: number;
}

export interface ShadowClient extends PrimaryClient {
  /** Resolves when everything queued so far has been applied to dg. */
  flush(): Promise<void>;
  readonly stats: ShadowStats;
}

const NONDETERMINISTIC = /\b(random|randomblob|changes|last_insert_rowid|total_changes)\s*\(|\b(current_timestamp|current_date|current_time)\b|['"]now['"]/i;

function sqlOf(statement: PrimaryStatement): string {
  return typeof statement === 'string' ? statement : statement.sql;
}

/** A primary value in dg's encoding, or `undefined` when there is none. */
function toDg(value: PrimaryValue): SqlValue | undefined {
  if (value === null || value === undefined) return null;
  if (typeof value === 'boolean' || typeof value === 'string') return value;
  if (typeof value === 'number') return Number.isFinite(value) ? value : undefined;
  if (typeof value === 'bigint') {
    // dg's JSON numbers are exact only within ±2^53; beyond that the
    // value would change in transit, so it is reported, not sent.
    return value >= BigInt(Number.MIN_SAFE_INTEGER) && value <= BigInt(Number.MAX_SAFE_INTEGER) ? Number(value) : undefined;
  }
  // libsql stores a Date as its millisecond timestamp.
  if (value instanceof Date) return value.getTime();
  if (value instanceof ArrayBuffer) return Array.from(new Uint8Array(value));
  if (value instanceof Uint8Array) return Array.from(value);
  return undefined;
}

/** The statement in dg's shape, or the reason it cannot be mirrored. */
export function toDgStatement(statement: PrimaryStatement): Statement | string {
  if (typeof statement === 'string') return { sql: statement, args: [] };
  const { sql, args } = statement;
  if (args === undefined) return { sql, args: [] };
  if (!Array.isArray(args)) return 'named parameters are not supported by dg; use positional ? arguments';
  const converted: SqlValue[] = [];
  for (const [i, arg] of args.entries()) {
    const value = toDg(arg);
    if (value === undefined) return `argument ${i + 1} (${typeof arg}) has no exact dg encoding`;
    converted.push(value);
  }
  return { sql, args: converted };
}

/** A row from either side, normalised so the two compare as data. */
function normaliseRow(row: ArrayLike<unknown>): unknown[] {
  return Array.from(row, (value) => {
    if (typeof value === 'bigint') return Number(value);
    if (value instanceof ArrayBuffer) return Array.from(new Uint8Array(value));
    if (value instanceof Uint8Array) return Array.from(value);
    return value;
  });
}

function sameRows(a: ArrayLike<unknown>[], b: ArrayLike<unknown>[]): boolean {
  return JSON.stringify(a.map(normaliseRow)) === JSON.stringify(b.map(normaliseRow));
}

function describe(error: unknown): string {
  if (error instanceof GatewayError) return `${error.code}: ${error.message}`;
  return error instanceof Error ? error.message : String(error);
}

/**
 * Wrap a `@libsql/client`-shaped client so its writes are mirrored to a dg
 * database. The returned object is a drop-in replacement for `execute` and
 * `batch`; the primary's results and errors pass through untouched.
 */
export function withShadow(primary: PrimaryClient, shadow: Database, options: ShadowOptions = {}): ShadowClient {
  const sampleReads = Math.min(Math.max(options.sampleReads ?? 0, 0), 1);
  const report = options.onDivergence ?? ((d: Divergence) => console.warn('dg shadow divergence', d));
  const maxQueue = options.maxQueue ?? 10_000;
  const stats: ShadowStats = { mirroredWrites: 0, comparedReads: 0, divergences: 0, dropped: 0, queued: 0 };
  let tail: Promise<void> = Promise.resolve();

  const diverge = (kind: DivergenceKind, sql: string, detail: string) => {
    stats.divergences += 1;
    try { report({ kind, sql, detail }); } catch { /* a reporter's failure is not the application's */ }
  };

  // One chain: each mirrored operation starts after the previous one has
  // settled, so dg sees writes in the order the primary acknowledged them.
  const enqueue = (work: () => Promise<void>, sql: string) => {
    if (stats.queued >= maxQueue) {
      stats.dropped += 1;
      diverge('shadow_error', sql, `shadow queue full (${maxQueue}); operation dropped, the copies have diverged`);
      return;
    }
    stats.queued += 1;
    tail = tail.then(work).catch(() => undefined).finally(() => { stats.queued -= 1; });
  };

  const mirrorWrite = (statements: PrimaryStatement[], results: PrimaryResult[]) => {
    const converted: Statement[] = [];
    for (const statement of statements) {
      const sql = sqlOf(statement);
      if (NONDETERMINISTIC.test(sql)) {
        diverge('nondeterministic', sql, 'depends on time or randomness; dg will compute its own value');
      }
      const dg = toDgStatement(statement);
      if (typeof dg === 'string') {
        diverge('unsupported', sql, dg);
        return;
      }
      converted.push(dg);
    }
    const label = converted.map((s) => s.sql).join('; ');
    enqueue(async () => {
      let got: QueryResult[];
      try {
        const first = converted[0];
        got = converted.length === 1 && first ? [await shadow.execute(first.sql, first.args)] : (await shadow.batch(converted)).results;
      } catch (error) {
        diverge('shadow_error', label, describe(error));
        return;
      }
      stats.mirroredWrites += converted.length;
      got.forEach((result, i) => {
        const expected = results[i]?.rowsAffected;
        if (expected !== undefined && result.rows_affected !== expected) {
          diverge('rows_affected_mismatch', converted[i]?.sql ?? label, `primary ${expected}, dg ${result.rows_affected}`);
        }
      });
    }, label);
  };

  const compareRead = (statement: PrimaryStatement, result: PrimaryResult) => {
    const sql = sqlOf(statement);
    const dg = toDgStatement(statement);
    if (typeof dg === 'string') return;
    enqueue(async () => {
      try {
        const got = await shadow.execute(dg.sql, dg.args);
        stats.comparedReads += 1;
        if (!sameRows(result.rows, got.rows)) {
          diverge('read_mismatch', sql, `primary ${JSON.stringify(result.rows.map(normaliseRow)).slice(0, 300)} dg ${JSON.stringify(got.rows).slice(0, 300)}`);
        }
      } catch (error) {
        diverge('shadow_error', sql, describe(error));
      }
    }, sql);
  };

  return {
    async execute(statement) {
      const result = await primary.execute(statement);
      const sql = sqlOf(statement);
      if (!isReadOnly(sql)) mirrorWrite([statement], [result]);
      else if (sampleReads > 0 && Math.random() < sampleReads) compareRead(statement, result);
      return result;
    },
    async batch(statements, mode) {
      const results = await primary.batch(statements, mode);
      if (!statements.every((s) => isReadOnly(sqlOf(s)))) mirrorWrite(statements, results);
      return results;
    },
    flush() {
      return tail;
    },
    get stats() {
      return { ...stats };
    },
  };
}

export interface TableComparison {
  table: string;
  primaryRows: number;
  shadowRows: number;
  identical: boolean;
  /** The first rows that differ, in table order: [primary, dg]. */
  firstDifferences: [unknown[] | null, unknown[] | null][];
}

export interface ComparisonReport {
  identical: boolean;
  /** Tables only one side has, or whose definitions differ. */
  schemaDifferences: string[];
  tables: TableComparison[];
}

const PAGE = 500;

function quote(name: string): string {
  return `"${name.replaceAll('"', '""')}"`;
}

/**
 * Compare every table of the primary with the dg copy, row by row, in a
 * deterministic order (every column ascending). Reads are paged, so tables
 * of any size compare within dg's per-response row cap.
 */
export async function compareDatabases(primary: PrimaryClient, shadow: Database, options: { tables?: string[]; maxDifferences?: number } = {}): Promise<ComparisonReport> {
  const internal = (name: string) => /^(sqlite_|_dg_|libsql_|_litestream|_cf_)/i.test(name);
  const schemaSql = "SELECT name, sql FROM sqlite_master WHERE type = 'table' ORDER BY name";
  const primarySchema = new Map((await primary.execute(schemaSql)).rows.map(normaliseRow).map((r) => [String(r[0]), String(r[1])] as const).filter(([n]) => !internal(n)));
  const shadowSchema = new Map((await shadow.execute(schemaSql)).rows.map((r) => [String(r[0]), String(r[1])] as const).filter(([n]) => !internal(n)));
  const schemaDifferences: string[] = [];
  for (const [name, sql] of primarySchema) {
    if (!shadowSchema.has(name)) schemaDifferences.push(`${name}: only on the primary`);
    else if (shadowSchema.get(name) !== sql) schemaDifferences.push(`${name}: definitions differ`);
  }
  for (const name of shadowSchema.keys()) if (!primarySchema.has(name)) schemaDifferences.push(`${name}: only on dg`);

  const maxDifferences = options.maxDifferences ?? 5;
  const tables: TableComparison[] = [];
  for (const table of options.tables ?? [...primarySchema.keys()].filter((t) => shadowSchema.has(t))) {
    const columns = (await primary.execute(`SELECT * FROM ${quote(table)} LIMIT 0`)).columns;
    const order = columns.map((_, i) => String(i + 1)).join(', ') || '1';
    const comparison: TableComparison = { table, primaryRows: 0, shadowRows: 0, identical: true, firstDifferences: [] };
    for (let offset = 0; ; offset += PAGE) {
      const sql = `SELECT * FROM ${quote(table)} ORDER BY ${order} LIMIT ${PAGE} OFFSET ${offset}`;
      const a = (await primary.execute(sql)).rows.map(normaliseRow);
      const b = (await shadow.execute(sql)).rows as unknown[][];
      comparison.primaryRows += a.length;
      comparison.shadowRows += b.length;
      for (let i = 0; i < Math.max(a.length, b.length); i += 1) {
        if (JSON.stringify(a[i] ?? null) !== JSON.stringify(b[i] ?? null)) {
          comparison.identical = false;
          if (comparison.firstDifferences.length < maxDifferences) comparison.firstDifferences.push([a[i] ?? null, b[i] ?? null]);
        }
      }
      if (a.length < PAGE && b.length < PAGE) break;
    }
    tables.push(comparison);
  }
  return { identical: schemaDifferences.length === 0 && tables.every((t) => t.identical), schemaDifferences, tables };
}
