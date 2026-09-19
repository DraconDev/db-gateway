//! Server metadata is separate from application SQL. Credentials are generated
//! locally by `dg init`/owner-only key issuance and only their SHA-256 hashes persist.
use crate::error::{Error, Result};
use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock, RwLockReadGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;

pub struct Store {
    root: PathBuf,
    pub global: Arc<Semaphore>,
    queues: Mutex<HashMap<String, Arc<Semaphore>>>,
    // All server requests hold a read lease until their connections close.
    // Purge takes an exclusive lease before unlinking SQLite files.
    lifecycle: RwLock<()>,
}

/// An operation exclusively owns this directory and all SQLite sidecars in it.
/// Drop cleans up on every success/error path, without touching another request.
pub struct Staging {
    _directory: tempfile::TempDir,
    path: PathBuf,
}
impl Staging {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DatabaseScope {
    name: String,
    generation: String,
}

#[derive(Clone)]
pub struct Principal {
    pub hash: String,
    pub role: String,
    /// Names are retained for the public inventory shape; authorization uses
    /// the private generation-bound scopes below.
    pub databases: Vec<String>,
    scopes: Vec<DatabaseScope>,
}
impl Principal {
    pub fn owner(&self) -> Result<()> {
        if self.role == "owner" {
            Ok(())
        } else {
            Err(Error::new(403, "forbidden", "owner required"))
        }
    }
    fn database(&self, name: &str, generation: Option<&str>) -> Result<()> {
        let allowed = self.role == "owner"
            || self.scopes.iter().any(|scope| {
                scope.name == name
                    && generation
                        .map(|current| scope.generation == current)
                        .unwrap_or(true)
            });
        if allowed {
            Ok(())
        } else {
            Err(Error::new(
                403,
                "database_forbidden",
                "database access denied",
            ))
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRequest {
    pub name: String,
    pub role: String,
    pub databases: Vec<String>,
}
pub fn hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}
fn generate_key() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        || name.starts_with('_')
    {
        return Err(Error::invalid());
    }
    Ok(())
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(|_| Error::internal())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| Error::internal())?;
        }
        let store = Self {
            root: root.to_path_buf(),
            global: Arc::new(Semaphore::new(4)),
            queues: Mutex::new(HashMap::new()),
            lifecycle: RwLock::new(()),
        };
        let mut metadata = store.meta()?;
        metadata.execute_batch("PRAGMA journal_mode=WAL;")?;
        migrate_metadata(&mut metadata, &store.root)?;
        Ok(store)
    }
    fn meta(&self) -> Result<Connection> {
        let conn = Connection::open(self.root.join("metadata.sqlite"))?;
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA cache_size=-1024;")?;
        Ok(conn)
    }
    pub fn bootstrap(&self) -> Result<String> {
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row(
            "SELECT count(*) FROM credentials WHERE role='owner'",
            [],
            |r| r.get::<_, i64>(0),
        )? > 0
        {
            return Err(Error::new(
                409,
                "already_bootstrapped",
                "owner already exists",
            ));
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES ('owner',?,'owner','[]')",
            [hash(&key)],
        )?;
        tx.commit()?;
        Ok(key)
    }
    pub fn authenticate(&self, key: &str) -> Result<Principal> {
        if key.len() != 64 {
            return Err(unauthorized());
        }
        let hash = hash(key);
        let result = self.meta()?.query_row(
            "SELECT role,databases FROM credentials WHERE hash=?",
            [&hash],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        );
        match result {
            Ok((role, dbs)) => Ok(Principal {
                hash,
                role,
                databases: serde_json::from_str(&dbs).map_err(|_| Error::internal())?,
            }),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(unauthorized()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn issue(&self, request: KeyRequest) -> Result<String> {
        validate_name(&request.name)?;
        if request.name == "owner"
            || !matches!(
                request.role.as_str(),
                "database_read_only" | "database_read_write"
            )
            || request.databases.is_empty()
            || request.databases.len() > 32
        {
            return Err(Error::invalid());
        }
        for name in &request.databases {
            self.path(name)?;
        }
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM credentials", [], |r| {
            r.get::<_, i64>(0)
        })? >= 256
        {
            return Err(Error::busy());
        }
        let key = generate_key();
        tx.execute(
            "INSERT INTO credentials VALUES (?,?,?,?)",
            params![
                request.name,
                hash(&key),
                request.role,
                serde_json::to_string(&request.databases).map_err(|_| Error::internal())?
            ],
        )
        .map_err(|_| Error::new(409, "key_exists", "credential name already exists"))?;
        tx.commit()?;
        Ok(key)
    }
    /// Owner-visible credential inventory: name, role, and scope.
    /// Hashes never leave the server; the owner row itself is listed.
    pub fn list_keys(&self) -> Result<serde_json::Value> {
        let conn = self.meta()?;
        let mut stmt =
            conn.prepare("SELECT name, role, databases FROM credentials ORDER BY name")?;
        let keys = stmt
            .query_map([], |r| {
                let databases: String = r.get(2)?;
                Ok(serde_json::json!({
                    "name": r.get::<_, String>(0)?,
                    "role": r.get::<_, String>(1)?,
                    "databases": serde_json::from_str::<serde_json::Value>(&databases)
                        .unwrap_or(serde_json::Value::Null),
                }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(serde_json::json!({"keys": keys}))
    }
    pub fn revoke(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        if name == "owner" {
            return Err(Error::invalid());
        }
        self.meta()?.execute(
            "DELETE FROM credentials WHERE name=? AND role != 'owner'",
            [name],
        )?;
        Ok(())
    }
    pub fn list(&self, principal: &Principal) -> Result<Vec<String>> {
        let conn = self.meta()?;
        let mut stmt =
            conn.prepare("SELECT name FROM databases WHERE deleted_at IS NULL ORDER BY name")?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names
            .into_iter()
            .filter(|name| principal.database(name).is_ok())
            .collect())
    }
    /// Owner-visible database facts: size, user tables with row counts,
    /// creation time, and tombstone state. Tombstoned names report their
    /// retention deadline instead of table detail; unknown names are 404.
    pub fn info(&self, name: &str) -> Result<serde_json::Value> {
        validate_name(name)?;
        let row: Option<(Option<i64>, Option<i64>, Option<i64>)> = self
            .meta()?
            .query_row(
                "SELECT deleted_at, purge_after, created_at FROM databases WHERE name=?",
                [name],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some((deleted_at, purge_after, created_at)) = row else {
            return Err(Error::new(404, "database_not_found", "database not found"));
        };
        let path = self.root.join(format!("{name}.sqlite"));
        let size_bytes = database_size_bytes(&path);
        if deleted_at.is_some() {
            return Ok(serde_json::json!({
                "name": name,
                "tombstoned": true,
                "purge_after": purge_after,
                "size_bytes": size_bytes,
            }));
        }
        let db = Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| Error::internal())?;
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND substr(name,1,4)!='_dg_' ORDER BY name")?
            .query_map([], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let mut detail = Vec::with_capacity(tables.len());
        for table in tables {
            let quoted = table.replace('"', "\"\"");
            let rows: i64 =
                db.query_row(&format!("SELECT count(*) FROM \"{quoted}\""), [], |r| {
                    r.get(0)
                })?;
            detail.push(serde_json::json!({"name": table, "rows": rows}));
        }
        Ok(serde_json::json!({
            "name": name,
            "tombstoned": false,
            "size_bytes": size_bytes,
            "created_at": created_at,
            "tables": detail,
        }))
    }
    pub fn create(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))? >= 128 {
            return Err(Error::busy());
        }
        let path = self.root.join(format!("{name}.sqlite"));
        // create_new means an orphan after a crash is never overwritten. An
        // unregistered file needs operator recovery, not silent adoption.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|_| {
                Error::new(
                    409,
                    "database_exists",
                    "database file exists or cannot be created",
                )
            })?;
        drop(file);
        let result = (|| {
            crate::execution::initialize(&path)?;
            tx.execute(
                "INSERT INTO databases (name, created_at) VALUES (?, ?)",
                rusqlite::params![name, now_secs()?],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            // Initialization and registry publication are one logical create:
            // never leave a file behind when either pre-commit step fails.
            remove_database_files(&path);
        }
        result
    }
    pub fn path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let found: bool = self.meta()?.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE name=? AND deleted_at IS NULL)",
            [name],
            |r| r.get(0),
        )?;
        if !found {
            return Err(Error::new(404, "database_not_found", "database not found"));
        }
        let path = self.root.join(format!("{name}.sqlite"));
        if !path.is_file() {
            return Err(Error::internal());
        }
        Ok(path)
    }
    pub fn queue(&self, name: &str) -> Result<Arc<Semaphore>> {
        // Call only after path() verifies a bounded registry entry.
        let mut queues = self.queues.lock().map_err(|_| Error::internal())?;
        Ok(queues
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone())
    }

    /// SQLite online backup of `name` to `dest`, consistent during writes.
    /// Pages are copied in bounded steps so concurrent commits interleave;
    /// the snapshot reflects one committed point in time.
    pub fn export_to(&self, name: &str, dest: &Path) -> Result<()> {
        let src_path = self.path(name)?;
        if dest.exists() {
            return Err(Error::new(
                409,
                "file_exists",
                "destination file already exists",
            ));
        }
        // Deliberately no per-database write-slot acquisition: the SQLite
        // online backup API on a WAL source produces a consistent committed
        // snapshot while writes continue (the copy restarts if the source
        // changes between steps). Bounding the step count keeps a
        // pathological concurrent-writer restart loop finite.
        let src =
            Connection::open_with_flags(&src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        src.busy_timeout(Duration::from_millis(250))?;
        let mut dst = Connection::open(dest)?;
        dst.busy_timeout(Duration::from_millis(250))?;
        let backup = Backup::new(&src, &mut dst)?;
        let mut result = StepResult::More;
        let mut steps = 0;
        while result == StepResult::More {
            steps += 1;
            if steps > 512 {
                result = StepResult::Busy;
                break;
            }
            result = backup.step(64).map_err(crate::error::Error::from)?;
        }
        drop(backup);
        match result {
            StepResult::Done => {
                dst.pragma_update(None, "journal_mode", "DELETE")?;
                Ok(())
            }
            _ => {
                drop(dst);
                remove_database_files(dest);
                Err(Error::new(
                    429,
                    "capacity_exceeded",
                    "backup could not complete; source busy",
                ))
            }
        }
    }

    pub fn request_lease(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.lifecycle.read().map_err(|_| Error::internal())
    }

    fn staging(&self) -> Result<Staging> {
        let directory = tempfile::Builder::new()
            .prefix(".lifecycle-")
            .tempdir_in(&self.root)
            .map_err(|_| Error::internal())?;
        let path = directory.path().join("snapshot.sqlite");
        Ok(Staging {
            _directory: directory,
            path,
        })
    }

    /// Snapshot in a private directory, automatically removed by its owner.
    pub fn export_snapshot(&self, name: &str) -> Result<Staging> {
        let temp = self.staging()?;
        self.export_to(name, temp.path())?;
        Ok(temp)
    }

    pub fn import_staging(&self, name: &str) -> Result<Staging> {
        validate_name(name)?;
        self.staging()
    }

    /// Tombstone `name`: registry row keeps the file under lock so the
    /// name can neither be queried nor recreated. Physical purge is delayed
    /// `retention_secs` and recorded via `purge_after`.
    pub fn delete(&self, name: &str, retention_secs: i64) -> Result<()> {
        validate_name(name)?;
        let now = now_secs()?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE databases SET deleted_at=?2, purge_after=?3
             WHERE name=?1 AND deleted_at IS NULL",
            params![name, now, now + retention_secs],
        )?;
        if changed == 0 {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM databases WHERE name=?)",
                [name],
                |r| r.get(0),
            )?;
            return Err(if exists {
                Error::new(409, "database_exists", "database already deleted")
            } else {
                Error::new(404, "database_not_found", "database not found")
            });
        }
        tx.commit()?;
        Ok(())
    }

    /// Physically remove files for tombstones whose retention elapsed.
    /// Idempotent, driven by the server maintenance loop even when idle.
    /// Skip a busy tick; never unlink while any request owns a connection.
    pub fn purge_expired(&self) -> Result<Vec<String>> {
        let _exclusive = match self.lifecycle.try_write() {
            Ok(lease) => lease,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(Vec::new()),
            Err(_) => return Err(Error::internal()),
        };
        let now = now_secs()?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let due: Vec<String> = tx
            .prepare("SELECT name FROM databases WHERE deleted_at IS NOT NULL AND purge_after<=?")?
            .query_map([now], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut purged = Vec::new();
        for name in due {
            let base = self.root.join(format!("{name}.sqlite"));
            for suffix in ["", "-wal", "-shm"] {
                let path = path_with_suffix(&base, suffix);
                if path.exists() {
                    std::fs::remove_file(&path).map_err(|_| Error::internal())?;
                }
            }
            tx.execute(
                "DELETE FROM databases WHERE name=? AND deleted_at IS NOT NULL",
                [&name],
            )?;
            purged.push(name);
        }
        tx.commit()?;
        Ok(purged)
    }

    /// Validate `source` and publish it as the new database `name` in one
    /// atomic registry step; a failed import leaves no partial database.
    pub fn import_from(&self, name: &str, source: &Path) -> Result<()> {
        validate_name(name)?;
        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))? >= 128 {
            return Err(Error::busy());
        }
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM databases WHERE name=?)",
            [name],
            |r| r.get(0),
        )?;
        if exists {
            // Covers both live databases and tombstones: importing over a
            // tombstoned row would silently overwrite the retained file.
            return Err(Error::new(409, "database_exists", "database exists"));
        }
        let path = self.root.join(format!("{name}.sqlite"));
        // Validate in private staging, then publish without overwriting orphans.
        let staged = self.staging()?;
        std::fs::copy(source, staged.path()).map_err(|_| Error::invalid())?;
        crate::execution::validate_import(staged.path())?;
        // The gateway requires its own replay schema on every managed file;
        // external SQLite databases gain it before publication (idempotent).
        crate::execution::initialize(staged.path())?;
        std::fs::hard_link(staged.path(), &path).map_err(|_| {
            Error::new(
                409,
                "database_exists",
                "database file exists or cannot be published",
            )
        })?;
        let result = (|| {
            tx.execute(
                "INSERT INTO databases (name, created_at) VALUES (?, ?)",
                rusqlite::params![name, now_secs()?],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if result.is_err() {
            remove_database_files(&path);
        }
        result
    }

    /// Clone `source` into new database `dest` via the online backup API;
    /// the copy is validated and registered atomically by import_from.
    pub fn clone_database(&self, source: &str, dest: &str) -> Result<()> {
        validate_name(dest)?;
        let temp = self.export_snapshot(source)?;
        self.import_from(dest, temp.path())
    }
}

fn migrate_metadata(conn: &mut Connection, root: &Path) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS databases(name TEXT PRIMARY KEY);",
    )?;
    for (column, declaration) in [
        ("deleted_at", "INTEGER"),
        ("purge_after", "INTEGER"),
        ("created_at", "INTEGER"),
    ] {
        let exists: bool = tx
            .prepare("SELECT 1 FROM pragma_table_info('databases') WHERE name=?")?
            .exists([column])?;
        if !exists {
            // Only the explicit absence case is benign. Every ALTER error is
            // returned so startup cannot proceed with a partial registry.
            tx.execute_batch(&format!(
                "ALTER TABLE databases ADD COLUMN {column} {declaration};"
            ))?;
        }
    }
    // Backfill creation timestamps for databases predating the column: file
    // mtime is the best surviving signal, 0 when the file is gone.
    let stale: Vec<String> = tx
        .prepare("SELECT name FROM databases WHERE created_at IS NULL")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for name in stale {
        let stamp = file_mtime_secs(&root.join(format!("{name}.sqlite")));
        tx.execute(
            "UPDATE databases SET created_at=? WHERE name=?",
            rusqlite::params![stamp, name],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn now_secs() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| Error::internal())
}

fn path_with_suffix(base: &Path, suffix: &str) -> PathBuf {
    let mut s = base.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

fn remove_database_files(base: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let p = path_with_suffix(base, suffix);
        let _ = std::fs::remove_file(&p);
    }
}

/// Best-effort file modification time as unix seconds; 0 when unknown.
fn file_mtime_secs(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Live database size: main file plus any WAL sidecars.
fn database_size_bytes(path: &Path) -> u64 {
    ["", "-wal", "-shm"]
        .iter()
        .filter_map(|s| std::fs::metadata(path_with_suffix(path, s)).ok())
        .map(|m| m.len())
        .sum()
}

pub fn unauthorized() -> Error {
    Error::new(
        401,
        "invalid_credential",
        "valid bearer credential required",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_publication_leaves_no_partial_database_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("src").unwrap();
        let path = store.path("src").unwrap();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t (v) VALUES ('alpha'),('beta')",
        ] {
            crate::execution::execute(
                &path,
                &[crate::execution::Statement {
                    sql: sql.into(),
                    args: vec![],
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap();
        }
        // Copy the snapshot bytes aside and release staging before the
        // failure, so any remaining `.lifecycle-` directory is a real leak.
        let source = dir.path().join("source.sqlite");
        {
            let snapshot = store.export_snapshot("src").unwrap();
            std::fs::copy(snapshot.path(), &source).unwrap();
        }
        // Deterministic publish failure: an orphan file (as left by an
        // interrupted publish) makes the hard_link syscall fail with a real
        // filesystem error after staging validation already passed. No host
        // mounts, permissions, or disk capacity are touched.
        let orphan = store.root.join("blocked.sqlite");
        std::fs::write(&orphan, b"orphan bytes").unwrap();
        let failed = store.import_from("blocked", &source);
        assert_eq!(failed.unwrap_err().code, "database_exists");
        // No partial publication: no registry row, no queryable database,
        // the orphan file is not clobbered, and staging cleaned itself up.
        let meta = Connection::open(store.root.join("metadata.sqlite")).unwrap();
        let rows: i64 = meta
            .query_row(
                "SELECT count(*) FROM databases WHERE name='blocked'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(
            store.path("blocked").unwrap_err().code,
            "database_not_found"
        );
        assert_eq!(std::fs::read(&orphan).unwrap(), b"orphan bytes");
        assert!(
            !std::fs::read_dir(&store.root).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".lifecycle-")),
            "failed import must not leak staging directories"
        );
        // Recovery: removing the orphan lets the identical import succeed
        // with exact data.
        std::fs::remove_file(&orphan).unwrap();
        store.import_from("blocked", &source).unwrap();
        let published = store.path("blocked").unwrap();
        let count = crate::execution::execute(
            &published,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }

    #[test]
    fn failed_create_removes_unregistered_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        let metadata = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        metadata
            .execute_batch(
                "CREATE TRIGGER reject_create BEFORE INSERT ON databases
                 WHEN NEW.name='broken'
                 BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
            )
            .unwrap();
        drop(metadata);

        assert!(store.create("broken").is_err());
        assert!(!dir.path().join("broken.sqlite").exists());
        assert_eq!(store.path("broken").unwrap_err().code, "database_not_found");
    }

    #[test]
    fn interrupted_publish_exposes_nothing_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.sqlite");
        {
            let store = Store::open(dir.path()).unwrap();
            store.bootstrap().unwrap();
            store.create("src").unwrap();
            let path = store.path("src").unwrap();
            for sql in [
                "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
                "INSERT INTO t (v) VALUES ('alpha'),('beta')",
            ] {
                crate::execution::execute(
                    &path,
                    &[crate::execution::Statement {
                        sql: sql.into(),
                        args: vec![],
                    }],
                    false,
                    false,
                    "owner",
                    None,
                )
                .unwrap();
            }
            let snapshot = store.export_snapshot("src").unwrap();
            std::fs::copy(snapshot.path(), &source).unwrap();
        }
        // Simulate a crash between file publication (hard_link) and the
        // registry commit: the file exists with no registry row. This is the
        // exact on-disk state of an interrupted import_from.
        let orphan = dir.path().join("interrupted.sqlite");
        std::fs::copy(&source, &orphan).unwrap();
        // Restart: reopen the store against the same directory.
        let store = Store::open(dir.path()).unwrap();
        // No partial exposure: the unregistered file is invisible.
        assert_eq!(
            store.path("interrupted").unwrap_err().code,
            "database_not_found"
        );
        // No silent overwrite or adoption: creation refuses, and the orphan
        // bytes are untouched.
        assert_eq!(
            store.create("interrupted").unwrap_err().code,
            "database_exists"
        );
        assert_eq!(
            std::fs::read(&orphan).unwrap(),
            std::fs::read(&source).unwrap()
        );
        // A later valid operation on another name still succeeds.
        store.import_from("elsewhere", &source).unwrap();
        // Operator recovery: remove the orphan, re-import, exact data.
        std::fs::remove_file(&orphan).unwrap();
        store.import_from("interrupted", &source).unwrap();
        let published = store.path("interrupted").unwrap();
        let count = crate::execution::execute(
            &published,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }

    #[test]
    fn metadata_migration_propagates_backfill_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut conn = Connection::open(dir.path().join("metadata.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
             CREATE TABLE databases(name TEXT PRIMARY KEY, deleted_at INTEGER, purge_after INTEGER, created_at INTEGER);
             INSERT INTO databases(name) VALUES ('legacy');
             CREATE TRIGGER reject_backfill BEFORE UPDATE ON databases BEGIN SELECT RAISE(ABORT, 'blocked'); END;",
        )
        .unwrap();
        let error = migrate_metadata(&mut conn, dir.path()).unwrap_err();
        assert_eq!(error.code, "invalid_sql");
    }

    #[test]
    fn clone_import_delete_roundtrip_at_store_level() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("src").unwrap();
        let path = store.path("src").unwrap();
        for sql in [
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)",
            "INSERT INTO t (v) VALUES ('alpha'),('beta')",
        ] {
            crate::execution::execute(
                &path,
                &[crate::execution::Statement {
                    sql: sql.into(),
                    args: vec![],
                }],
                false,
                false,
                "owner",
                None,
            )
            .unwrap();
        }
        // Clone via store API only.
        let result = store.clone_database("src", "copy1");
        if let Err(ref e) = result {
            panic!("clone failed: {e}");
        }
        let copy = store.path("copy1").unwrap();
        let count = crate::execution::execute(
            &copy,
            &[crate::execution::Statement {
                sql: "SELECT count(*) FROM t".into(),
                args: vec![],
            }],
            false,
            false,
            "owner",
            None,
        )
        .unwrap();
        assert_eq!(count["rows"][0][0], 2);
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;

    #[test]
    fn request_owned_staging_survives_same_name_operations() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("source").unwrap();
        let first = store.export_snapshot("source").unwrap();
        let bytes = std::fs::read(first.path()).unwrap();
        let second = store.export_snapshot("source").unwrap();
        assert_ne!(first.path(), second.path());
        drop(second);
        assert_eq!(std::fs::read(first.path()).unwrap(), bytes);
        let import_a = store.import_staging("target").unwrap();
        std::fs::write(import_a.path(), &bytes).unwrap();
        let import_b = store.import_staging("target").unwrap();
        std::fs::write(import_b.path(), b"invalid").unwrap();
        assert_ne!(import_a.path(), import_b.path());
        assert!(store.import_from("target", import_b.path()).is_err());
        drop(import_b);
        store.import_from("target", import_a.path()).unwrap();
        assert_eq!(std::fs::read(import_a.path()).unwrap(), bytes);
    }

    #[test]
    fn purge_defers_until_inflight_request_closes_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create("active").unwrap();
        let request = store.request_lease().unwrap();
        let path = store.path("active").unwrap();
        let mut writer = Connection::open(&path).unwrap();
        let tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        tx.execute_batch("CREATE TABLE pending(v); INSERT INTO pending VALUES (1);")
            .unwrap();
        store.delete("active", 0).unwrap();
        // Actual lease and transaction remain live, not a timing assumption.
        assert!(store.purge_expired().unwrap().is_empty());
        assert!(path.exists());
        assert_eq!(store.path("active").unwrap_err().code, "database_not_found");
        tx.commit().unwrap();
        drop(writer);
        drop(request);
        assert_eq!(store.purge_expired().unwrap(), vec!["active"]);
        store.create("active").unwrap();
        assert!(store.purge_expired().unwrap().is_empty());
        assert!(store.path("active").unwrap().exists());
    }

    #[test]
    fn tombstone_purges_after_retention_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.bootstrap().unwrap();
        store.create("doomed").unwrap();
        let path = store.path("doomed").unwrap();
        assert!(path.exists());
        // Tombstone with 0-second retention; purge must remove file + row.
        store.delete("doomed", 0).unwrap();
        let purged = store.purge_expired().unwrap();
        assert_eq!(purged, vec!["doomed".to_string()]);
        assert!(
            !path.exists(),
            "file must be physically removed after retention"
        );
        assert!(!path.with_extension("sqlite-wal").exists());
        // Registry row is gone: recreate works.
        store.create("doomed").unwrap();
    }
}
