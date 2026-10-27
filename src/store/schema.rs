//! The structure of one database's tables: columns, indexes, foreign keys.
//!
//! This is the answer to "what is in this table" at the level of the
//! schema rather than the rows. The row grid says which columns a
//! table has; this says what each column is -- its declared type,
//! whether it may be null, whether it is the primary key -- and what
//! indexes and foreign keys the table carries.
//!
//! It is read server-side, from the database file itself, with SQLite's
//! own PRAGMAs. The client cannot do this: the query endpoint refuses
//! PRAGMA (`sql_forbidden`), because a PRAGMA can change a connection's
//! behaviour, and the answer an operator wants here is a fact about the
//! schema, not a statement to run. The gateway already opens the file
//! read-only for `info`, so it can read the schema the same way.
//!
//! Reading the PRAGMAs beats parsing the CREATE statement: SQLite knows
//! which column is the primary key of a composite key, which index came
//! from a UNIQUE constraint, and what the declared type really is. A
//! parser would have to guess all three.

use rusqlite::Connection;
use serde_json::{Value, json};
use std::path::Path;

use crate::error::{Error, Result};
use crate::store::Store;

/// The tables an operator sees in one database.
///
/// The same list `dg info` shows: the gateway's own `_dg_` bookkeeping
/// stays hidden and everything else the file holds is listed. It is
/// written once, here, so the table list and the structure cannot
/// disagree about which tables a database has.
pub fn user_tables(conn: &Connection) -> Result<Vec<String>> {
    let tables = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type='table' \
             AND lower(substr(name,1,4))!='_dg_' ORDER BY name",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(tables)
}

/// One table's structure, as the gateway reports it.
pub struct TableStructure {
    /// The table's name.
    pub name: String,
    /// How many rows it holds.
    pub rows: i64,
    /// Its columns, in declaration order.
    pub columns: Vec<Column>,
    /// Its indexes.
    pub indexes: Vec<Index>,
    /// Its foreign keys.
    pub foreign_keys: Vec<ForeignKey>,
    /// The statement that created it, verbatim.
    pub sql: Option<String>,
}

/// One column of a table.
pub struct Column {
    /// The column's name.
    pub name: String,
    /// Its declared type, as written in the schema ("" when untyped).
    pub kind: String,
    /// Whether the schema forbids a null.
    pub not_null: bool,
    /// Whether this column is part of the primary key.
    pub primary_key: bool,
    /// Its default expression, when it has one.
    pub default: Option<String>,
}

/// One index of a table.
pub struct Index {
    /// The index's name.
    pub name: String,
    /// Whether it is unique.
    pub unique: bool,
    /// The columns it covers, in order.
    pub columns: Vec<String>,
}

/// One foreign key of a table.
pub struct ForeignKey {
    /// The table it points at.
    pub table: String,
    /// The column it points from.
    pub from: String,
    /// The column it points at ("" when it points at the primary key).
    pub to: String,
    /// What happens on update.
    pub on_update: String,
    /// What happens on delete.
    pub on_delete: String,
}

/// The structure of every table in one database file.
pub fn structure(path: &Path) -> Result<Vec<TableStructure>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| Error::internal())?;
    let mut structures = Vec::new();
    for name in user_tables(&conn)? {
        structures.push(table_structure(&conn, &name)?);
    }
    Ok(structures)
}

/// One table's structure, read with SQLite's own PRAGMAs.
fn table_structure(conn: &Connection, name: &str) -> Result<TableStructure> {
    let quoted = quote(name);
    let rows: i64 = conn.query_row(&format!("SELECT count(*) FROM {quoted}"), [], |row| {
        row.get(0)
    })?;
    let sql: Option<String> = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")?
        .query_row([name], |row| row.get(0))
        .ok();
    Ok(TableStructure {
        name: name.to_string(),
        rows,
        columns: columns(conn, &quoted)?,
        indexes: indexes(conn, &quoted)?,
        foreign_keys: foreign_keys(conn, &quoted)?,
        sql,
    })
}

/// The columns of one table, as `PRAGMA table_info` reports them.
fn columns(conn: &Connection, quoted: &str) -> Result<Vec<Column>> {
    let columns = conn
        .prepare(&format!("PRAGMA table_info({quoted})"))?
        .query_map([], |row| {
            Ok(Column {
                name: row.get(1)?,
                kind: row.get(2)?,
                not_null: row.get::<_, i64>(3)? != 0,
                primary_key: row.get::<_, i64>(5)? != 0,
                default: row.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<Column>, _>>()?;
    Ok(columns)
}

/// The indexes of one table, with the columns each one covers.
///
/// `PRAGMA index_list` names the indexes and says whether they are
/// unique; `PRAGMA index_info` says which columns they cover. An
/// index over an expression has no column names, and says so.
fn indexes(conn: &Connection, quoted: &str) -> Result<Vec<Index>> {
    let listed = conn
        .prepare(&format!("PRAGMA index_list({quoted})"))?
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(2)? != 0))
        })?
        .collect::<std::result::Result<Vec<(String, bool)>, _>>()?;
    let mut indexes = Vec::with_capacity(listed.len());
    for (name, unique) in listed {
        let covered = conn
            .prepare(&format!("PRAGMA index_info({})", quote(&name)))?
            .query_map([], |row| row.get::<_, Option<String>>(2))?
            .collect::<std::result::Result<Vec<Option<String>>, _>>()?;
        indexes.push(Index {
            name,
            unique,
            columns: covered
                .into_iter()
                .flatten()
                .collect::<Vec<String>>(),
        });
    }
    Ok(indexes)
}

/// The foreign keys of one table, as `PRAGMA foreign_key_list` reports them.
fn foreign_keys(conn: &Connection, quoted: &str) -> Result<Vec<ForeignKey>> {
    let keys = conn
        .prepare(&format!("PRAGMA foreign_key_list({quoted})"))?
        .query_map([], |row| {
            Ok(ForeignKey {
                table: row.get(2)?,
                from: row.get(3)?,
                to: row.get(4)?,
                on_update: row.get(5)?,
                on_delete: row.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<ForeignKey>, _>>()?;
    Ok(keys)
}

/// A name quoted for SQLite.
///
/// The name comes from the file's own schema, so it is not attacker
/// controlled, but a quote inside it would still break the statement
/// out of its identifier: it is doubled, the way SQLite expects.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Store {
    /// The structure of one database's tables.
    ///
    /// Owner-only, like the database's own listing: the answer says
    /// what is inside a database, and the credential that may open a
    /// database is the owner's.
    pub fn schema(&self, name: &str) -> Result<Value> {
        let Some((deleted_at, purge_after, created_at)) = self.registry_entry(name)? else {
            return Err(Error::new(404, "database_not_found", "database not found"));
        };
        let path = self.root.join(format!("{name}.sqlite"));
        if deleted_at.is_some() {
            return Ok(json!({
                "database": name,
                "tombstoned": true,
                "purge_after": purge_after,
                "created_at": created_at,
                "tables": [],
            }));
        }
        let structures = structure(&path)?;
        Ok(json!({
            "database": name,
            "tombstoned": false,
            "created_at": created_at,
            "tables": structures.iter().map(table_document).collect::<Vec<Value>>(),
        }))
    }
}

/// One table's structure as the gateway's JSON.
fn table_document(table: &TableStructure) -> Value {
    json!({
        "name": table.name,
        "rows": table.rows,
        "columns": table.columns.iter().map(|column| json!({
            "name": column.name,
            "type": column.kind,
            "not_null": column.not_null,
            "primary_key": column.primary_key,
            "default": column.default,
        })).collect::<Vec<Value>>(),
        "indexes": table.indexes.iter().map(|index| json!({
            "name": index.name,
            "unique": index.unique,
            "columns": index.columns,
        })).collect::<Vec<Value>>(),
        "foreign_keys": table.foreign_keys.iter().map(|key| json!({
            "table": key.table,
            "from": key.from,
            "to": key.to,
            "on_update": key.on_update,
            "on_delete": key.on_delete,
        })).collect::<Vec<Value>>(),
        "sql": table.sql,
    })
}
