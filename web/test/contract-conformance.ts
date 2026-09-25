import type { DatabaseInfo as ClientDatabaseInfo, KeyInfo as ClientKeyInfo, QueryResult as ClientQueryResult, TableInfo as ClientTableInfo } from '../../client/src/index.js';
import type { DatabaseInfo, KeyInfo, QueryResult, TableInfo } from '../src/api.js';

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
type Assert<T extends true> = T;

type _QueryResult = Assert<Equal<QueryResult, ClientQueryResult>>;
type _KeyInfo = Assert<Equal<KeyInfo, ClientKeyInfo>>;
type _TableInfo = Assert<Equal<TableInfo, ClientTableInfo>>;
type _DatabaseInfo = Assert<Equal<DatabaseInfo, ClientDatabaseInfo>>;

export type ContractConformance = true;
