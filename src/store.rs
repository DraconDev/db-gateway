//! Server metadata is separate from application SQL. Credentials are generated
//! locally by `dg init`/owner-only key issuance and only their SHA-256 hashes persist.
use crate::error::{Error, Result};
use rusqlite::{Connection, params};
use rusqlite::backup::{Backup, StepResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Semaphore;

pub struct Store {
    root: PathBuf,
    pub global: Arc<Semaphore>,
    queues: Mutex<HashMap<String, Arc<Semaphore>>>,
}
#[derive(Clone)]
pub struct Principal {
    pub hash: String,
    pub role: String,
    pub databases: Vec<String>,
}
impl Principal {
    pub fn owner(&self) -> Result<()> {
        if self.role == "owner" {
            Ok(())
        } else {
            Err(Error::new(403, "forbidden", "owner required"))
        }
    }
    pub fn database(&self, name: &str) -> Result<()> {
        if self.role == "owner" || self.databases.iter().any(|db| db == name) {
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
        };
        store.meta()?.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL); CREATE TABLE IF NOT EXISTS databases(name TEXT PRIMARY KEY);")?;
        // M2 migration: tombstone support for safe deletion. ALTER fails
        // harmlessly when the columns already exist.
        for column in ["deleted_at INTEGER", "purge_after INTEGER"] {
            let _ = store
                .meta()?
                .execute_batch(&format!("ALTER TABLE databases ADD COLUMN {column};"));
        }
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
        let mut stmt = conn.prepare("SELECT name FROM databases ORDER BY name")?;
        let names = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names
            .into_iter()
            .filter(|name| principal.database(name).is_ok())
            .collect())
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
        crate::execution::initialize(&path)?;
        tx.execute("INSERT INTO databases VALUES (?)", [name])?;
        tx.commit()?;
        Ok(())
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
        let queue = self.queue(name)?;
        let _permit = queue
            .try_acquire_owned()
            .map_err(|_| Error::busy())?;
        // Hold the per-database write slot for the whole backup so the
        // snapshot is a stable committed point and never torn.
        let src = Connection::open_with_flags(
            &src_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        src.busy_timeout(Duration::from_millis(250))?;
        let mut dst = Connection::open(dest)?;
        dst.busy_timeout(Duration::from_millis(250))?;
        let backup = Backup::new(&src, &mut dst)?;
        let mut result = StepResult::More;
        while result == StepResult::More {
            result = backup
                .step(64)
                .map_err(crate::error::Error::from)?;
        }
        drop(backup);
        match result {
            StepResult::Done => {
                dst.pragma_update(None, "journal_mode", "DELETE")?;
                Ok(())
            }
            _ => {
                drop(dst);
                let _ = std::fs::remove_file(dest);
                let _ = std::fs::remove_file(dest.with_extension("sqlite-wal"));
                let _ = std::fs::remove_file(dest.with_extension("sqlite-shm"));
                Err(Error::new(
                    429,
                    "capacity_exceeded",
                    "backup could not complete; source busy",
                ))
            }
        }
    }
}
pub fn unauthorized() -> Error {
    Error::new(
        401,
        "invalid_credential",
        "valid bearer credential required",
    )
}
