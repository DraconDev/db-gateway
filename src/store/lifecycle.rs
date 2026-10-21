use super::*;

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

pub(super) fn migrate_metadata(conn: &mut Connection, root: &Path) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials(name TEXT PRIMARY KEY, hash TEXT UNIQUE NOT NULL, role TEXT NOT NULL, databases TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS databases(name TEXT PRIMARY KEY, generation TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS write_markers(name TEXT PRIMARY KEY, written_at INTEGER NOT NULL);",
    )?;
    for (column, declaration) in [
        ("deleted_at", "INTEGER"),
        ("purge_after", "INTEGER"),
        ("created_at", "INTEGER"),
        // Existing registries need a nullable migration step because SQLite
        // cannot add a NOT NULL column to a table that already has rows.
        ("generation", "TEXT"),
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
    // Every registry row must have an opaque identity before credentials are
    // migrated. The identity survives tombstoning and disappears only when
    // the registry row is purged.
    let missing_generations: Vec<String> = tx
        .prepare("SELECT name FROM databases WHERE generation IS NULL OR generation=''")?
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for name in missing_generations {
        let generation = new_generation(&tx)?;
        tx.execute(
            "UPDATE databases SET generation=? WHERE name=?",
            params![generation, name],
        )?;
    }
    migrate_legacy_scopes(&tx)?;
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

/// fsync a directory so a name newly linked or created inside it survives a
/// crash. The data fsync on the file is not enough: the directory entry is a
/// separate write, and losing it after the registry row is durable leaves a
/// registered database with no file. Mirrors the CLI's publish_export_file.
fn sync_parent_dir(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| Error::internal())
}

pub(super) fn remove_database_files(base: &Path) {
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

impl Store {
    pub fn list(&self, principal: &Principal) -> Result<Vec<String>> {
        let conn = self.meta()?;
        let mut stmt = conn.prepare(
            "SELECT name,generation FROM databases WHERE deleted_at IS NULL ORDER BY name",
        )?;
        let names = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names
            .into_iter()
            .filter(|(name, generation)| principal.database(name, Some(generation)).is_ok())
            .map(|(name, _)| name)
            .collect())
    }

    /// How many databases are live and how many are tombstoned.
    ///
    /// The operator's first question about a service is "how much of
    /// what I made is still there", and the answer is two numbers, not
    /// a list. Tombstoned databases still occupy a name until the
    /// retention window purges them, so the count is not the same as
    /// the length of `list`.
    pub fn database_counts(&self) -> Result<(u64, u64)> {
        let conn = self.meta()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(SUM(CASE WHEN deleted_at IS NULL THEN 1 ELSE 0 END), 0), \
                    COALESCE(SUM(CASE WHEN deleted_at IS NOT NULL THEN 1 ELSE 0 END), 0) \
             FROM databases",
        )?;
        let counts = stmt.query_row([], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(counts)
    }

    /// Every live database, unscoped: the owner view, and the view backup
    /// freshness needs. Deliberately a separate statement from `list` rather
    /// than a shared one with the filter applied later -- `list` has to carry
    /// the generation column to keep a key bound to one generation of a
    /// name from seeing its successor, and folding that away to share code
    /// would quietly widen what an app key can read.
    pub fn list_all(&self) -> Result<Vec<String>> {
        let conn = self.meta()?;
        // A registry that has never been initialised has no table yet.
        // The owner creates it; a reader must not, so it reports "no
        // databases" instead of failing on a schema it is not allowed to
        // write.
        if !table_exists(&conn, "databases")? {
            return Ok(Vec::new());
        }
        let mut stmt =
            conn.prepare("SELECT name FROM databases WHERE deleted_at IS NULL ORDER BY name")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(names)
    }

    /// Owner-visible database facts: size, user tables with row counts,
    /// creation time, and tombstone state. Tombstoned names report their
    /// retention deadline instead of table detail; unknown names are 404.
    /// COUNT(*) is a per-page C walk (OP_Count), not a per-row scan, so it
    /// stays fast even on large tables; a VDBE progress deadline cannot
    /// bound it (no loop-edge opcodes) and is deliberately absent.
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
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND lower(substr(name,1,4))!='_dg_' ORDER BY name")?
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
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))?
            >= MAX_DATABASES
        {
            return Err(Error::busy());
        }
        let generation = new_generation(&tx)?;
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
            // Same publication ordering as import_from: the directory entry
            // that create_new just made must be durable before the registry
            // row naming it is committed, or a crash can leave the registry
            // pointing at a file that does not exist.
            std::fs::File::open(&path)
                .and_then(|handle| handle.sync_all())
                .map_err(|_| Error::internal())?;
            sync_parent_dir(&path)?;
            let created_at = now_secs()?;
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, created_at, generation],
            )?;
            tx.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, created_at],
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

    /// Run a write under the freshness fence, releasing the per-name state
    /// again when the operation failed without creating the database. Same
    /// begin/finish/cancel sequence the route handlers ran inline, with the
    /// release on every failure path (including a failed `begin_write`) so
    /// the invariant holds in one place instead of at each handler.
    pub fn fenced_write<F>(&self, name: &str, operation: F) -> Result<()>
    where
        F: FnOnce(&Self) -> Result<()>,
    {
        let previous_write = self.last_write(name);
        if let Err(error) = self.begin_write(name) {
            self.release_unused(name);
            return Err(error);
        }
        let result = operation(self);
        if result.is_ok() {
            self.finish_write(name);
        } else {
            self.cancel_write(name, previous_write);
            self.release_unused(name);
        }
        result
    }

    pub fn request_lease(&self) -> Result<RwLockReadGuard<'_, ()>> {
        self.lifecycle.read().map_err(|_| Error::internal())
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
                Error::new(409, "already_deleted", "database already deleted")
            } else {
                Error::new(404, "database_not_found", "database not found")
            });
        }
        tx.execute(
            "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
             ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
            params![name, now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Physically remove files for tombstones whose retention elapsed.
    /// Idempotent, driven by the server maintenance loop even when idle.
    /// Skip a busy tick; never unlink while any request owns a connection.
    pub fn purge_expired(&self) -> Result<Vec<String>> {
        let _exclusive = match self.lifecycle.try_write() {
            Ok(lease) => lease,
            // A contended tick is normal, not an error: the 1s cadence means
            // the next tick almost always succeeds. But a tombstone can then
            // sit past its retention window with no trace, so record the
            // deferral. Debug level keeps it free when nothing is wrong.
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::debug!("purge tick deferred; lifecycle lease busy");
                return Ok(Vec::new());
            }
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
            // The freshness marker is keyed by the same name, so a purge that
            // leaves the row behind grows metadata.sqlite without bound and
            // reloads every marker into memory on the next boot.
            tx.execute("DELETE FROM write_markers WHERE name=?", [&name])?;
            purged.push(name);
        }
        tx.commit()?;
        // Prune the per-name in-memory state for the names just purged. Safe
        // here and only here: the exclusive lifecycle lease held above is
        // shared by every admitted request, so none can be mid-admission with
        // this name's queue guard in hand. Removing the entry cannot split a
        // guard, because no other holder can exist at this point.
        for name in &purged {
            if let Ok(mut queues) = self.queues.lock() {
                queues.remove(name);
            }
            self.metrics.forget(name);
        }
        Ok(purged)
    }

    /// Validate `source` and publish it as the new database `name` in one
    /// atomic registry step; a failed import leaves no partial database.
    pub fn import_from(&self, name: &str, source: &Path) -> Result<()> {
        validate_name(name)?;
        // Copy, validate, and initialize without holding the metadata write
        // lock. The registry transaction below is only the short publication
        // step, so a large independent import cannot stall authentication and
        // all other registry operations.
        let path = self.root.join(format!("{name}.sqlite"));
        // Reject an already-full registry before copying an untrusted source.
        // The authoritative check is repeated in the publication transaction.
        {
            let conn = self.meta()?;
            let count: i64 = conn.query_row("SELECT count(*) FROM databases", [], |r| r.get(0))?;
            if count >= MAX_DATABASES {
                return Err(Error::busy());
            }
        }
        let staged = self.staging()?;
        // The import source may be a live SQLite database with a populated WAL
        // sidecar. Copying only the main file would validate an older committed
        // image and silently omit newer rows. The read-only backup API folds
        // the main database and its live sidecars into one consistent image;
        // it also makes the staged copy standalone before publication.
        let source_conn =
            Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|_| Error::invalid())?;
        let mut staged_conn = Connection::open(staged.path()).map_err(|_| Error::internal())?;
        let backup = Backup::new(&source_conn, &mut staged_conn).map_err(|_| Error::invalid())?;
        match backup.step(-1).map_err(|_| Error::invalid())? {
            StepResult::Done => {}
            _ => return Err(Error::invalid()),
        }
        drop(backup);
        drop(staged_conn);
        crate::execution::validate_import(staged.path())?;
        crate::execution::initialize(staged.path())?;

        let mut conn = self.meta()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.query_row("SELECT count(*) FROM databases", [], |r| r.get::<_, i64>(0))?
            >= MAX_DATABASES
        {
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
        let generation = new_generation(&tx)?;
        // Publication durability ordering, mirroring publish_export_file in
        // src/main.rs: make the staged bytes durable, link the name, fsync
        // the containing directory so the new entry is stable, and only then
        // commit the registry row. Without the directory fsync a crash can
        // leave the registry naming a database whose file never reached
        // stable storage, and `path()` then reports a registered database as
        // missing with no way to recover it short of a manual registry edit.
        std::fs::File::open(staged.path())
            .and_then(|file| file.sync_all())
            .map_err(|_| Error::internal())?;
        if std::fs::hard_link(staged.path(), &path).is_err() {
            // The link never appeared, so this process published nothing and
            // owns no file at `path`. Return the conflict and touch nothing:
            // a pre-existing file belongs to whoever put it there, and
            // create() applies the same rule ("an unregistered file needs
            // operator recovery, not silent adoption").
            return Err(Error::new(
                409,
                "database_exists",
                "database file exists or cannot be published",
            ));
        }
        // The link exists; now make its directory entry durable. This must
        // succeed BEFORE the registry row that names it is committed, or a
        // crash in between leaves the registry pointing at a file that never
        // reached stable storage -- `path()` only checks is_file(), so there
        // is no self-healing and no signal beyond a 500. Because the link is
        // ours, a failure here means we unlink it rather than stranding an
        // unregistered file that would block a later import of this name.
        if let Err(error) = sync_parent_dir(&path) {
            remove_database_files(&path);
            return Err(error);
        }
        let result = (|| {
            tx.execute(
                "INSERT INTO databases (name, created_at, generation) VALUES (?, ?, ?)",
                rusqlite::params![name, now_secs()?, generation],
            )?;
            tx.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, now_secs()?],
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

    pub(super) fn purge_expired_locked(&self, transfers: &mut HashMap<String, Transfer>) -> usize {
        let stale: Vec<String> = transfers
            .iter()
            .filter(|(_, t)| t.created.elapsed() >= self.transfer_ttl)
            .map(|(id, _)| id.clone())
            .collect();
        let count = stale.len();
        for id in stale {
            if let Some(t) = transfers.remove(&id) {
                remove_staging_dir(&t.dir);
            }
        }
        count
    }
}
