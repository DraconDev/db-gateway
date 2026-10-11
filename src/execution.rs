//! One connection per admitted request; all application SQL runs in a managed
//! transaction. The replay record commits in the SAME database transaction.
use crate::{
    error::{Error, Result},
    values::{json_to_sql, sql_to_json},
};
use rusqlite::{
    Connection, OpenFlags,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    params, params_from_iter,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

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
    // IF NOT EXISTS keeps clone (export of a database that already has the
    // table) idempotent; validation has already rejected foreign _dg_ tables,
    // so a recreated table here can only be the canonical one.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS _dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, hash TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(principal,key));")?;
    Ok(())
}
/// An imported file must be a standalone, fully consistent SQLite database
/// with no reference violations and no protected _dg_ tables of its own.
pub fn validate_import(path: &Path) -> Result<()> {
    if !path.is_file() {
        return Err(Error::invalid());
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::new(
            400,
            "invalid_request",
            "imported database failed integrity_check",
        ));
    }
    let bad: i64 = conn
        .prepare("SELECT count(*) FROM pragma_foreign_key_check")?
        .query_row([], |r| r.get(0))?;
    if bad > 0 {
        return Err(Error::new(
            400,
            "invalid_request",
            "imported database failed foreign_key_check",
        ));
    }
    let protected: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))='_dg_' ORDER BY name",
        )?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if protected
        .iter()
        .any(|name| !name.eq_ignore_ascii_case("_dg_replay"))
    {
        // Imported files must not smuggle server-owned tables. SQLite table
        // names are case-insensitive, so screen case variants too.
        return Err(Error::new(
            400,
            "invalid_request",
            "imported database contains protected tables",
        ));
    }
    // A file may carry its own _dg_replay (e.g. a gateway snapshot). It is
    // published only if the definition is the canonical one; initialize's
    // CREATE TABLE IF NOT EXISTS must never paper over a different schema.
    let replay_sql: Option<String> = match conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND lower(name)='_dg_replay'",
        [],
        |r| r.get(0),
    ) {
        Ok(sql) => Some(sql),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error.into()),
    };
    if replay_sql
        .map(|sql| normalize(&sql) != normalize(REPLAY_DDL))
        .unwrap_or(false)
    {
        return Err(Error::new(
            400,
            "invalid_request",
            "imported database has an incompatible _dg_replay schema",
        ));
    }
    Ok(())
}

/// Canonical replay schema: identical up to quoting/whitespace/case so that
/// gateway exports and freshly initialized files both validate.
const REPLAY_DDL: &str = "create table if not exists _dg_replay (principal text not null, key text not null, hash text not null, result text not null, created integer not null, primary key(principal,key))";

fn normalize(sql: &str) -> String {
    // Token comparison: split on whitespace and DDL punctuation so any
    // spacing, paren adjacency, or IF NOT EXISTS variant compares equal;
    // lowercase folds case differences. Inputs are real sqlite_master DDL,
    // so token streams are unambiguous.
    let tokens: Vec<&str> = sql
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',')
        .filter(|token| !token.is_empty())
        .collect();
    let mut kept = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        if tokens[index].eq_ignore_ascii_case("if")
            && tokens
                .get(index + 1)
                .is_some_and(|next| next.eq_ignore_ascii_case("not"))
            && tokens
                .get(index + 2)
                .is_some_and(|next| next.eq_ignore_ascii_case("exists"))
        {
            index += 3;
        } else {
            kept.push(tokens[index]);
            index += 1;
        }
    }
    kept.join(" ").to_ascii_lowercase()
}

#[cfg(test)]
mod import_schema_tests {
    use super::*;

    #[test]
    fn imported_replay_table_must_match_canonical_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.sqlite");
        let make = |ddl: &str| {
            let _ = std::fs::remove_file(&path);
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(ddl).unwrap();
            conn
        };
        // Canonical (as gateway initialize writes it, with IF NOT EXISTS).
        make(&format!(
            "CREATE TABLE {}_dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, hash TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(principal,key));",
            ""
        ));
        validate_import(&path).unwrap();
        // Same columns, different constraints: rejected.
        drop(make("CREATE TABLE _dg_replay (principal TEXT, key TEXT)"));
        assert!(validate_import(&path).is_err());
        // Extra column: rejected.
        drop(make(
            "CREATE TABLE _dg_replay (principal TEXT NOT NULL, key TEXT NOT NULL, hash TEXT NOT NULL, result TEXT NOT NULL, created INTEGER NOT NULL, extra TEXT, PRIMARY KEY(principal,key))",
        ));
        assert!(validate_import(&path).is_err());
        // Same schema, hostile spacing and mixed case: accepted.
        drop(make(
            "CREATE  TABLE\n\tIF  NOT  EXISTS  _DG_Replay (\n  principal   TEXT  NOT  NULL,\nkey TEXT NOT NULL,\nhash TEXT NOT NULL,\nresult TEXT NOT NULL,\ncreated INTEGER NOT NULL,\nPRIMARY KEY(principal,key)\n)",
        ));
        validate_import(&path).unwrap();
    }

    #[test]
    fn protected_import_tables_are_rejected_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("candidate.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE \"_DG_secret\" (value TEXT);")
            .unwrap();
        drop(conn);
        assert_eq!(validate_import(&path).unwrap_err().code, "invalid_request");

        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE \"_DG_secret\"; CREATE TABLE \"_DG_REPLAY\" (principal TEXT);",
        )
        .unwrap();
        drop(conn);
        assert_eq!(validate_import(&path).unwrap_err().code, "invalid_request");
    }

    #[test]
    fn foreign_key_violations_reject_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fk.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "PRAGMA foreign_keys=OFF;
             CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (pid INTEGER REFERENCES parent(id));
             INSERT INTO child VALUES (99);",
        )
        .unwrap();
        drop(conn);
        assert!(validate_import(&path).is_err());
    }
}
fn protected(name: &str) -> bool {
    name.to_ascii_lowercase().starts_with("_dg_")
}
fn authorize(ctx: AuthContext<'_>, read_only: bool) -> Authorization {
    use AuthAction::*;
    let allowed = match ctx.action {
        Read { table_name, .. } => !protected(table_name),
        Select | Recursive => true,
        Function { function_name } => !matches!(
            function_name.to_ascii_lowercase().as_str(),
            "load_extension" | "writefile" | "readfile"
        ),
        Insert { table_name } | Update { table_name, .. } | Delete { table_name } => {
            !read_only && !protected(table_name)
        }
        CreateTable { table_name } | DropTable { table_name } => {
            !read_only && !protected(table_name)
        }
        CreateIndex {
            index_name,
            table_name,
        }
        | DropIndex {
            index_name,
            table_name,
        } => !read_only && !protected(index_name) && !protected(table_name),
        // No user transactions, attachment, PRAGMAs, virtual tables, triggers,
        // views, temp objects or ALTER in M1. Unknown actions fail closed.
        _ => false,
    };
    if allowed {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}
fn run_statement(
    conn: &Connection,
    statement: &Statement,
    read_only: bool,
    remaining_result_bytes: usize,
) -> Result<Value> {
    if statement.sql.is_empty() || statement.sql.len() > 65536 || statement.args.len() > 256 {
        return Err(Error::invalid());
    }
    let args = statement
        .args
        .iter()
        .map(json_to_sql)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| Error::invalid())?;
    conn.authorizer(Some(move |ctx: AuthContext<'_>| authorize(ctx, read_only)))?;
    let result = (|| {
        let mut stmt = conn.prepare(&statement.sql)?;
        // prepare rejects stacked statements (rusqlite Error::MultipleStatement
        // → invalid_sql); only a lone terminator or trailing comment is
        // accepted after the first statement.
        if read_only && !stmt.readonly() {
            return Err(Error::new(403, "sql_forbidden", "read-only credential"));
        }
        let readonly = stmt.readonly();
        let columns: Vec<_> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = Vec::new();
        let mut bytes = 0;
        {
            let mut cursor = stmt.query(params_from_iter(args))?;
            while let Some(row) = cursor.next()? {
                if rows.len() >= MAX_ROWS {
                    return Err(result_limit());
                }
                let values: Vec<_> = (0..columns.len())
                    .map(|i| row.get_ref(i).map(sql_to_json))
                    .collect::<std::result::Result<_, _>>()?;
                bytes += serde_json::to_vec(&values)
                    .map_err(|_| Error::internal())?
                    .len();
                if bytes > remaining_result_bytes {
                    return Err(result_limit());
                }
                rows.push(values);
            }
        }
        Ok(
            json!({"columns":columns, "rows":rows, "rows_affected":if readonly {0} else {conn.changes()}}),
        )
    })();
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)?;
    result
}
fn result_limit() -> Error {
    Error::new(
        413,
        "result_limit_exceeded",
        "result exceeds bounded response size",
    )
}

/// Admission-lane pre-filter (NOT a security boundary): true when every
/// statement opens with SELECT/VALUES/EXPLAIN, or WITH without a
/// data-modifying keyword. Write enforcement still happens per-statement at
/// execution via the SQLite authorizer plus `stmt.readonly()`
/// (run_statement) — this only decides which waiting room a request sits
/// in. It fails closed: comments, odd heads, string literals containing
/// keywords, and anything ambiguous take the write lane — slower for that
/// request, never unsafe. (Prepare-based classification was tried: it fails
/// on every query naming an application table, since a scratch connection
/// has no schema, which would put all real reads on the write lane.)
pub fn is_read_only(statements: &[Statement]) -> bool {
    statements
        .iter()
        .all(|s| is_read_only_one(s.sql.trim_start()))
}
fn is_read_only_one(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    let head = upper.as_str();
    (head.starts_with("SELECT")
        || head.starts_with("VALUES")
        || head.starts_with("EXPLAIN")
        || head.starts_with("WITH"))
        && !has_write_keyword(head)
}
/// Whole-word scan for data-modifying keywords, so a WITH..SELECT whose
/// CTE is named `updated` stays on the read lane while WITH..INSERT does
/// not. False positives (a literal saying 'deleted') only cost the write
/// lane, never correctness.
fn has_write_keyword(sql: &str) -> bool {
    const WORDS: [&str; 9] = [
        "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "VACUUM", "REINDEX",
    ];
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &sql[start..i];
            if WORDS.contains(&word) {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}
/// Outcome of a statement execution, with the one bit the caller needs
/// beyond the payload: whether the response was served from the
/// idempotency-replay cache rather than by running the statements.
///
/// A replay commits nothing to the database, so a caller maintaining a
/// per-database write marker must treat it as a rolled-back request rather
/// than a successful write. See [`execute`].
#[derive(Debug)]
pub struct ExecuteOutcome {
    pub value: Value,
    pub replayed: bool,
}

pub fn execute(
    path: &Path,
    statements: &[Statement],
    batch: bool,
    read_only: bool,
    principal: &str,
    key: Option<&str>,
) -> Result<Value> {
    Ok(execute_detailed(path, statements, batch, read_only, principal, key)?.value)
}

/// Same as [`execute`], but reports whether the response was a replay. A
/// replay hit committed nothing, so a caller that fenced the request with
/// `begin_write` must undo the fence with `cancel_write` rather than close
/// it with `finish_write` — see `api::execute`.
pub fn execute_detailed(
    path: &Path,
    statements: &[Statement],
    batch: bool,
    read_only: bool,
    principal: &str,
    key: Option<&str>,
) -> Result<ExecuteOutcome> {
    if statements.is_empty() || statements.len() > 32 {
        return Err(Error::invalid());
    }
    if let Some(key) = key {
        if read_only {
            // Reads are naturally replayable; a key on them is meaningless,
            // so say that instead of a bare malformed-request error.
            return Err(Error::new(
                400,
                "idempotency_key_not_supported",
                "idempotency keys are only supported for write requests",
            ));
        }
        if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::invalid());
        }
    }
    let mut conn = open(path)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))?;
    // Deferred for statement-classified reads regardless of principal role:
    // an Immediate transaction takes a RESERVED lock even for a pure
    // SELECT, so concurrent reads would shed each other with SQLITE_BUSY
    // once the shared read guard lets them run together. Writers keep
    // Immediate to fail fast at BEGIN rather than mid-write.
    let tx = conn.transaction_with_behavior(if read_only || is_read_only(statements) {
        rusqlite::TransactionBehavior::Deferred
    } else {
        rusqlite::TransactionBehavior::Immediate
    })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::internal())?
        .as_secs() as i64;
    let hash = hex::encode(Sha256::digest(
        serde_json::to_vec(&(batch, statements)).map_err(|_| Error::internal())?,
    ));
    if let Some(key) = key {
        // Deletion and insertion are transactional; reject on capacity rather
        // than evict an unexpired key and accidentally execute it twice.
        tx.execute(
            "DELETE FROM _dg_replay WHERE created <= ?",
            [now - RETENTION],
        )?;
        let replay = tx.query_row(
            "SELECT hash,result FROM _dg_replay WHERE principal=? AND key=?",
            params![principal, key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );
        match replay {
            Ok((old_hash, result)) => {
                if old_hash != hash {
                    // The retention purge above must survive even on the
                    // error path; otherwise a conflicting key pins dead rows.
                    tx.commit()?;
                    return Err(Error::new(
                        409,
                        "idempotency_key_conflict",
                        "key reused with different request",
                    ));
                }
                // A replay hit returns early without executing anything, but
                // the retention purge it already ran lives in this same
                // transaction: commit first so expired rows are actually
                // pruned instead of rolled back on every replayed key.
                let value: Value = serde_json::from_str(&result).map_err(|_| Error::internal())?;
                tx.commit()?;
                return Ok(ExecuteOutcome {
                    value,
                    replayed: true,
                });
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error.into()),
        }
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM _dg_replay WHERE principal=?",
            params![principal],
            |r| r.get(0),
        )?;
        if count >= MAX_REPLAYS {
            return Err(Error::busy());
        }
    }
    let mut results = Vec::new();
    // Charge every statement against one shared response budget. The small
    // reserve covers JSON punctuation and column metadata; the exact encoded
    // response is checked once before commit.
    let mut remaining_result_bytes = MAX_RESULT.saturating_sub(4096);
    for statement in statements {
        let result = run_statement(&tx, statement, read_only, remaining_result_bytes)?;
        remaining_result_bytes = remaining_result_bytes.saturating_sub(
            serde_json::to_vec(&result)
                .map_err(|_| Error::internal())?
                .len(),
        );
        results.push(result);
    }
    let result = if batch {
        json!({"results":results})
    } else {
        results.remove(0)
    };
    let encoded = serde_json::to_string(&result).map_err(|_| Error::internal())?;
    if encoded.len() > MAX_RESULT {
        return Err(result_limit());
    }
    if let Some(key) = key {
        tx.execute(
            "INSERT INTO _dg_replay VALUES (?,?,?,?,?)",
            params![principal, key, hash, encoded, now],
        )?;
    }
    tx.commit()?;
    Ok(ExecuteOutcome {
        value: result,
        replayed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lane_classifier_marks_only_reads() {
        let s = |sql: &str| Statement {
            sql: sql.into(),
            args: vec![],
        };
        assert!(is_read_only(&[s("SELECT 1")]));
        assert!(is_read_only(&[s("SELECT a FROM t WHERE b = ?")]));
        assert!(is_read_only(&[
            s("SELECT 1"),
            s("WITH x AS (SELECT 1) SELECT * FROM x")
        ]));
        // CTE name containing a keyword as a substring stays on the read lane.
        assert!(is_read_only(&[s(
            "WITH updated AS (SELECT 1) SELECT * FROM updated"
        )]));
        assert!(!is_read_only(&[s("INSERT INTO t VALUES (1)")]));
        assert!(!is_read_only(&[s("UPDATE t SET a = 1)")]));
        assert!(!is_read_only(&[s("DELETE FROM t")]));
        assert!(!is_read_only(&[s("CREATE TABLE t (a)")]));
        assert!(!is_read_only(&[
            s("SELECT 1"),
            s("INSERT INTO t VALUES (1)")
        ]));
        // WITH..INSERT is a write despite the WITH head.
        assert!(!is_read_only(&[s(
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x"
        )]));
        // Anything else fails closed to the write lane.
        assert!(!is_read_only(&[s("SELEKT 1")]));
        // Unknown tables still take the read lane — execution rejects them
        // with invalid_sql; the lane never decides validity.
        assert!(is_read_only(&[s("SELECT * FROM no_such_table")]));
        assert!(!is_read_only(&[s(
            "SELECT * FROM t WHERE note = 'do not delete me'"
        )]));
    }
    #[test]
    fn invalid_sql_and_arguments_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str, args| Statement {
            sql: sql.into(),
            args,
        };
        // Invalid SQL: syntax error.
        assert_eq!(
            execute(&path, &[s("SELEKT 1", vec![])], false, false, "owner", None)
                .unwrap_err()
                .code,
            "invalid_sql"
        );
        // Valid SQL, wrong parameter count.
        assert_eq!(
            execute(
                &path,
                &[s("SELECT 1, 2", vec![json!(1)])],
                false,
                false,
                "owner",
                None
            )
            .unwrap_err()
            .code,
            "invalid_sql"
        );
        // Unsupported argument type.
        assert_eq!(
            execute(
                &path,
                &[s("SELECT 1", vec![json!({"a": 1})])],
                false,
                false,
                "owner",
                None
            )
            .unwrap_err()
            .code,
            "invalid_request"
        );
        // Protected server table is not writable by application SQL.
        assert_eq!(
            execute(
                &path,
                &[s(
                    "INSERT INTO _dg_replay VALUES ('x','y','z','w',1)",
                    vec![]
                )],
                false,
                false,
                "owner",
                None
            )
            .unwrap_err()
            .code,
            "sql_forbidden"
        );
        // Idempotency keys are meaningless on reads; the rejection says so.
        assert_eq!(
            execute(
                &path,
                &[s("SELECT 1", vec![])],
                false,
                true,
                "reader",
                Some("k1")
            )
            .unwrap_err()
            .code,
            "idempotency_key_not_supported"
        );
    }

    #[test]
    fn stacked_statements_are_rejected_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str| Statement {
            sql: sql.into(),
            args: vec![],
        };
        execute(
            &path,
            &[s("CREATE TABLE t (a INTEGER)")],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        execute(
            &path,
            &[s("INSERT INTO t VALUES (1)")],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        // A second statement hiding behind a semicolon is rejected, not run.
        assert_eq!(
            execute(
                &path,
                &[s("SELECT 1; DELETE FROM t")],
                false,
                false,
                "owner",
                None
            )
            .unwrap_err()
            .code,
            "invalid_sql"
        );
        let rows = execute(
            &path,
            &[s("SELECT count(*) FROM t")],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(rows["rows"], json!([[1]]));
        // A lone terminator or trailing comment is still a single statement.
        assert!(execute(&path, &[s("SELECT 1;")], false, false, "owner", None).is_ok());
        assert!(
            execute(
                &path,
                &[s("SELECT 1; -- trailing")],
                false,
                false,
                "owner",
                None
            )
            .is_ok()
        );
    }

    #[test]
    fn statement_count_sql_length_and_argument_count_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: String| Statement { sql, args: vec![] };
        // Batch statement count: 32 admitted, 33 and empty rejected.
        let ok: Vec<Statement> = (0..32).map(|_| s("SELECT 1".into())).collect();
        assert!(execute(&path, &ok, true, false, "owner", None).is_ok());
        let too_many: Vec<Statement> = (0..33).map(|_| s("SELECT 1".into())).collect();
        assert_eq!(
            execute(&path, &too_many, true, false, "owner", None)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        assert_eq!(
            execute(&path, &[], true, false, "owner", None)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        // SQL text length: exactly 65536 bytes admitted, 65537 rejected.
        let exact = format!("SELECT '{}'", "x".repeat(65536 - 9));
        assert_eq!(exact.len(), 65536);
        assert!(execute(&path, &[s(exact)], false, false, "owner", None).is_ok());
        let over = format!("SELECT '{}'", "x".repeat(65537 - 9));
        assert_eq!(over.len(), 65537);
        assert_eq!(
            execute(&path, &[s(over)], false, false, "owner", None)
                .unwrap_err()
                .code,
            "invalid_request"
        );
        // Argument count: 256 admitted, 257 rejected (single result column
        // so the 128-column output cap does not mask the argument boundary).
        let placeholders = |n: usize| vec!["?"; n].join("+");
        let args = |n: usize| (1..=n as i64).map(Value::from).collect::<Vec<_>>();
        assert!(
            execute(
                &path,
                &[Statement {
                    sql: format!("SELECT {}", placeholders(256)),
                    args: args(256),
                }],
                false,
                false,
                "owner",
                None,
            )
            .is_ok()
        );
        assert_eq!(
            execute(
                &path,
                &[Statement {
                    sql: format!("SELECT {}", placeholders(257)),
                    args: args(257),
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap_err()
            .code,
            "invalid_request"
        );
    }

    #[test]
    fn deadline_interrupts_expensive_query_and_database_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str, args| Statement {
            sql: sql.into(),
            args,
        };
        execute(
            &path,
            &[s("CREATE TABLE heavy (v INTEGER)", vec![])],
            true,
            false,
            "owner",
            None,
        )
        .unwrap();
        // A deliberately expensive query: recursive CTE far past the 2s deadline.
        let expensive = "INSERT INTO heavy WITH RECURSIVE c(v) AS (SELECT 1 UNION ALL SELECT v+1 FROM c) SELECT v FROM c";
        let error =
            execute(&path, &[s(expensive, vec![])], false, false, "owner", None).unwrap_err();
        assert_eq!(error.code, "query_deadline_exceeded");
        // The interrupted write must have rolled back and the database must
        // still serve new requests.
        let result = execute(
            &path,
            &[s("INSERT INTO heavy VALUES (1)", vec![])],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(result["rows_affected"], 1);
        let count = execute(
            &path,
            &[s("SELECT count(*) FROM heavy", vec![])],
            false,
            true,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(
            count["rows"],
            json!([[1]]),
            "interrupted write must not have committed"
        );
    }

    #[test]
    fn idempotency_capacity_is_per_principal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str| Statement {
            sql: sql.into(),
            args: vec![],
        };
        execute(
            &path,
            &[s("CREATE TABLE n (v INTEGER)")],
            true,
            false,
            "owner",
            None,
        )
        .unwrap();
        // Fill principal A to the documented per-principal cap.
        for i in 0..MAX_REPLAYS {
            execute(
                &path,
                &[s("INSERT INTO n VALUES (1)")],
                false,
                false,
                "writer_a",
                Some(&format!("key-{i}")),
            )
            .unwrap_or_else(|e| panic!("write {i}: {e}"));
        }
        // Principal A at cap: further keyed writes are rejected...
        assert_eq!(
            execute(
                &path,
                &[s("INSERT INTO n VALUES (9)")],
                false,
                false,
                "writer_a",
                Some("overflow")
            )
            .unwrap_err()
            .code,
            "capacity_exceeded"
        );
        // ...but principal B is unaffected (regression: capacity was global).
        let status_key = execute(
            &path,
            &[s("INSERT INTO n VALUES (1)")],
            false,
            false,
            "writer_b",
            Some("b-first"),
        );
        assert!(
            status_key.is_ok(),
            "other principal must not be locked out: {status_key:?}"
        );
        // Replay at capacity still replays instead of failing (same key + identical body).
        let replay = execute(
            &path,
            &[s("INSERT INTO n VALUES (1)")],
            false,
            false,
            "writer_a",
            Some("key-0"),
        );
        assert!(replay.is_ok());
    }

    #[test]
    fn contention_and_parallel_independent_databases() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..4)
            .map(|i| {
                let p = dir.path().join(format!("db{i}.sqlite"));
                initialize(&p).unwrap();
                execute(
                    &p,
                    &[Statement {
                        sql: "CREATE TABLE t (v INTEGER)".into(),
                        args: vec![],
                    }],
                    true,
                    false,
                    "owner",
                    None,
                )
                .unwrap();
                p
            })
            .collect();
        // Parallel writers against independent databases must all succeed;
        // per-database limits are enforced at the API layer (one permit per
        // database), so the core must not corrupt or serialize across files.
        let handles: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for n in 0..20 {
                        execute(
                            &path,
                            &[Statement {
                                sql: "INSERT INTO t VALUES (?)".into(),
                                args: vec![json!(n)],
                            }],
                            false,
                            false,
                            "owner",
                            Some(&format!("db{i}-key-{n}")),
                        )?;
                    }
                    Ok::<(), crate::error::Error>(())
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        for (i, path) in paths.iter().enumerate() {
            let count = execute(
                path,
                &[Statement {
                    sql: "SELECT count(*) FROM t".into(),
                    args: vec![],
                }],
                false,
                true,
                "owner",
                None,
            )
            .unwrap();
            assert_eq!(count["rows"], json!([[20]]), "db{i}");
        }
    }

    #[test]
    fn sqlite_parameterized_round_trip_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str, args| Statement {
            sql: sql.into(),
            args,
        };
        execute(
            &path,
            &[
                s("CREATE TABLE t(v TEXT)", vec![]),
                s("INSERT INTO t VALUES (?)", vec![json!("aria")]),
            ],
            true,
            false,
            "owner",
            Some("seed"),
        )
        .unwrap();
        let result = execute(
            &path,
            &[s("SELECT v FROM t", vec![])],
            false,
            true,
            "reader",
            None,
        )
        .unwrap();
        assert_eq!(result["rows"], json!([["aria"]]));
    }

    #[test]
    fn read_only_credentials_reject_idempotency_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str| Statement {
            sql: sql.into(),
            args: vec![],
        };
        // A keyed request on a read-only credential is rejected with a
        // dedicated code, not a replay: reads need no dedupe and no key
        // slot is stored for them.
        assert_eq!(
            execute(
                &path,
                &[s("SELECT 1")],
                false,
                true,
                "reader",
                Some("read-key-1")
            )
            .unwrap_err()
            .code,
            "idempotency_key_not_supported"
        );
        // The identical keyed shape works for a writer, proving only the
        // read-only combination is rejected.
        execute(
            &path,
            &[s("SELECT 1")],
            false,
            false,
            "writer",
            Some("read-key-1"),
        )
        .unwrap();
    }

    #[test]
    fn execute_detailed_reports_replayed_on_idempotency_hit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sqlite");
        initialize(&path).unwrap();
        let s = |sql: &str| Statement {
            sql: sql.into(),
            args: vec![],
        };
        execute(
            &path,
            &[s("CREATE TABLE replay_det (v INTEGER)")],
            true,
            false,
            "owner",
            None,
        )
        .unwrap();
        // Fresh write: replayed is false, payload carries rows_affected.
        let fresh = execute_detailed(
            &path,
            &[s("INSERT INTO replay_det VALUES (1)")],
            false,
            false,
            "owner",
            Some("replay-det-1"),
        )
        .unwrap();
        assert!(!fresh.replayed);
        assert_eq!(fresh.value["rows_affected"], 1);
        // Same key + same body: replayed is true and the value is the cached
        // JSON; nothing ran in this request.
        let replay = execute_detailed(
            &path,
            &[s("INSERT INTO replay_det VALUES (1)")],
            false,
            false,
            "owner",
            Some("replay-det-1"),
        )
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.value, fresh.value);
        // A non-keyed write reports replayed = false (no cache lookup).
        let unkeyed = execute_detailed(
            &path,
            &[s("INSERT INTO replay_det VALUES (2)")],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert!(!unkeyed.replayed);
        assert_eq!(unkeyed.value["rows_affected"], 1);
    }
}
