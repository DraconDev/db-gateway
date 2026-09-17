//! One connection per admitted request; all application SQL runs in a managed
//! transaction. The replay record commits in the SAME database transaction.
use std::{path::Path, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use rusqlite::{Connection, OpenFlags, params, params_from_iter, hooks::{AuthAction, AuthContext, Authorization}, limits::Limit};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use crate::{error::{Error, Result}, values::{json_to_sql, sql_to_json}};

pub const MAX_RESULT: usize = 256 * 1024;
pub const MAX_ROWS: usize = 1000;
const RETENTION: i64 = 86400;
const MAX_REPLAYS: i64 = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub sql: String,
    #[serde(default)]
    pub args: Vec<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    pub statements: Vec<Statement>,
    pub atomic: bool,
}
fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    conn.busy_timeout(Duration::from_millis(250))?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF; PRAGMA cache_size=-2048; PRAGMA temp_store=MEMORY;")?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 64 * 1024)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 128)?;
    conn.set_limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER, 256)?;
    conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
    conn.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 100000)?;
    Ok(conn)
}
pub fn initialize(path: &Path) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE _dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, hash TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(principal,key));")?;
    Ok(())
}
fn protected(name: &str) -> bool { name.to_ascii_lowercase().starts_with("_dg_") }
fn authorize(ctx: AuthContext<'_>, read_only: bool) -> Authorization {
    use AuthAction::*;
    let allowed = match ctx.action {
        Read { table_name, .. } => !protected(table_name),
        Select | Recursive => true,
        Function { function_name } => !matches!(function_name.to_ascii_lowercase().as_str(), "load_extension" | "writefile" | "readfile"),
        Insert { table_name } | Update { table_name, .. } | Delete { table_name } => !read_only && !protected(table_name),
        CreateTable { table_name } | DropTable { table_name } => !read_only && !protected(table_name),
        CreateIndex { index_name, table_name } | DropIndex { index_name, table_name } => !read_only && !protected(index_name) && !protected(table_name),
        // No user transactions, attachment, PRAGMAs, virtual tables, triggers,
        // views, temp objects or ALTER in M1. Unknown actions fail closed.
        _ => false,
    };
    if allowed { Authorization::Allow } else { Authorization::Deny }
}
fn run_statement(conn: &Connection, statement: &Statement, read_only: bool) -> Result<Value> {
    if statement.sql.is_empty() || statement.sql.len() > 65536 || statement.args.len() > 256 { return Err(Error::invalid()); }
    let args = statement.args.iter().map(json_to_sql).collect::<std::result::Result<Vec<_>, _>>().map_err(|_| Error::invalid())?;
    conn.authorizer(Some(move |ctx: AuthContext<'_>| authorize(ctx, read_only)))?;
    let result = (|| {
        let mut stmt = conn.prepare(&statement.sql)?;
        // rusqlite's extra_check rejects multiple statements during prepare.
        if read_only && !stmt.readonly() { return Err(Error::new(403, "sql_forbidden", "read-only credential")); }
        let readonly = stmt.readonly();
        let columns: Vec<_> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = Vec::new();
        let mut bytes = 0;
        {
            let mut cursor = stmt.query(params_from_iter(args))?;
            while let Some(row) = cursor.next()? {
                if rows.len() >= MAX_ROWS { return Err(result_limit()); }
                let values: Vec<_> = (0..columns.len()).map(|i| row.get_ref(i).map(sql_to_json)).collect::<std::result::Result<_, _>>()?;
                bytes += serde_json::to_vec(&values).map_err(|_| Error::internal())?.len();
                if bytes > MAX_RESULT { return Err(result_limit()); }
                rows.push(values);
            }
        }
        Ok(json!({"columns":columns, "rows":rows, "rows_affected":if readonly {0} else {conn.changes()}}))
    })();
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}
fn result_limit() -> Error { Error::new(413, "result_limit_exceeded", "result exceeds bounded response size") }

pub fn execute(path: &Path, statements: &[Statement], batch: bool, read_only: bool, principal: &str, key: Option<&str>) -> Result<Value> {
    if statements.is_empty() || statements.len() > 32 { return Err(Error::invalid()); }
    if let Some(key) = key {
        if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) || read_only { return Err(Error::invalid()); }
    }
    let mut conn = open(path)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    let tx = conn.transaction_with_behavior(if read_only { rusqlite::TransactionBehavior::Deferred } else { rusqlite::TransactionBehavior::Immediate })?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|_| Error::internal())?.as_secs() as i64;
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(&(batch, statements)).map_err(|_| Error::internal())?));
    if let Some(key) = key {
        // Deletion and insertion are transactional; reject on capacity rather
        // than evict an unexpired key and accidentally execute it twice.
        tx.execute("DELETE FROM _dg_replay WHERE created <= ?", [now - RETENTION])?;
        let replay = tx.query_row("SELECT hash,result FROM _dg_replay WHERE principal=? AND key=?", params![principal,key], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)));
        match replay {
            Ok((old_hash, result)) => {
                if old_hash != hash { return Err(Error::new(409,"idempotency_key_conflict","key reused with different request")); }
                return serde_json::from_str(&result).map_err(|_| Error::internal());
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {},
            Err(error) => return Err(error.into()),
        }
        let count: i64 = tx.query_row("SELECT count(*) FROM _dg_replay", [], |r| r.get(0))?;
        if count >= MAX_REPLAYS { return Err(Error::busy()); }
    }
    let mut results = Vec::new();
    for statement in statements { results.push(run_statement(&tx, statement, read_only)?); }
    let result = if batch { json!({"results":results}) } else { results.remove(0) };
    let encoded = serde_json::to_string(&result).map_err(|_| Error::internal())?;
    if encoded.len() > MAX_RESULT { return Err(result_limit()); }
    if let Some(key) = key {
        tx.execute("INSERT INTO _dg_replay VALUES (?,?,?,?,?)", params![principal,key,hash,encoded,now])?;
    }
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sqlite_parameterized_round_trip_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str, args| Statement { sql:sql.into(), args };
        execute(&path, &[s("CREATE TABLE t(v TEXT)",vec![]),s("INSERT INTO t VALUES (?)",vec![json!("aria")])],true,false,"owner",Some("seed")).unwrap();
        let result = execute(&path,&[s("SELECT v FROM t",vec![])],false,true,"reader",None).unwrap();
        assert_eq!(result["rows"],json!([["aria"]]));
    }
}
