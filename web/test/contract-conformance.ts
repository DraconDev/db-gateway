import type { DatabaseInfo as ClientDatabaseInfo, DatabaseUsage as ClientDatabaseUsage, KeyInfo as ClientKeyInfo, QueryResult as ClientQueryResult, StatusResponse as ClientStatusResponse, TableInfo as ClientTableInfo } from '../../client/src/index.js';
import type { DatabaseInfo, DatabaseUsage, KeyInfo, QueryResult, StatusResponse, TableInfo } from '../src/api.js';

type Equal<A, B> = (<T>() => T extends A ? 1 : 2) extends (<T>() => T extends B ? 1 : 2) ? true : false;
type Assert<T extends true> = T;

type _QueryResult = Assert<Equal<QueryResult, ClientQueryResult>>;
type _KeyInfo = Assert<Equal<KeyInfo, ClientKeyInfo>>;
type _TableInfo = Assert<Equal<TableInfo, ClientTableInfo>>;
type _DatabaseInfo = Assert<Equal<DatabaseInfo, ClientDatabaseInfo>>;
type _DatabaseUsage = Assert<Equal<DatabaseUsage, ClientDatabaseUsage>>;
type _StatusResponse = Assert<Equal<StatusResponse, ClientStatusResponse>>;

export type ContractConformance = true;
