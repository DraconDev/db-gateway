use super::*;

/// Backup-set convention: versioned sets live next to the data dir unless
/// `--backup-root` overrides. Returns None for degenerate data-dir paths.
fn default_backup_root(data_dir: &Path) -> Option<PathBuf> {
    let parent = data_dir.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.join("dg-backups"))
}

/// Versioned-set directory names look like `20260922T223715Z` (the backup
/// script's `%Y%m%dT%H%M%SZ`). Staging dirs and anything else never match.
pub(crate) fn is_set_dir(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 16
        && b[..8].iter().all(|c| c.is_ascii_digit())
        && b[8] == b'T'
        && b[9..15].iter().all(|c| c.is_ascii_digit())
        && b[15] == b'Z'
}

/// One published versioned set (scripts/micro2-backup.sh layout): directory
/// name, filesystem mtime, member database names, registry presence, and the
/// conservative start boundary captured before any database export. A set may
/// be published after a concurrent write, so publication mtime is not proof
/// that the write was included; only a write strictly before this boundary is
/// eligible to be reported fresh.
pub struct BackupSet {
    pub timestamp: String,
    pub mtime: SystemTime,
    pub coverage_start: Option<SystemTime>,
    pub coverage_end: Option<SystemTime>,
    pub databases: Vec<String>,
    pub has_registry: bool,
}

fn load_write_markers_from_connection(conn: &Connection) -> Result<HashMap<String, SystemTime>> {
    let mut stmt = conn.prepare("SELECT name,written_at FROM write_markers")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut writes = HashMap::new();
    for row in rows {
        let (name, seconds) = row?;
        let Ok(seconds) = u64::try_from(seconds) else {
            continue;
        };
        if let Some(stamp) = UNIX_EPOCH.checked_add(Duration::from_secs(seconds)) {
            writes.insert(name, stamp);
        }
    }
    Ok(writes)
}

/// Manifests are kilobytes; anything larger is corruption or abuse, never
/// a real set. Both the metadata pre-check and the post-read length check
/// matter: the first avoids the allocation, the second closes the race.
pub(crate) fn read_manifest_capped(path: &Path) -> Option<Vec<u8>> {
    const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
    if std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0) > MAX_MANIFEST_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return None;
    }
    Some(bytes)
}

impl Store {
    /// Start a durable freshness interval before a mutation begins. The
    /// negative marker is intentionally written first: if the process dies
    /// after the data commit but before the positive marker, restart reports
    /// `unknown` rather than falsely fresh. If this fence cannot be persisted,
    /// the mutation is rejected before it can commit.
    pub fn begin_write(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        let _marker_guard = self.marker_lock.lock().map_err(|_| Error::internal())?;
        self.persist_marker_value(name, -1)?;
        if let Ok(mut writes) = self.writes.lock() {
            writes.remove(name);
        }
        Ok(())
    }

    /// Close a successful mutation's freshness interval. If the positive
    /// marker cannot be persisted, the pre-write negative marker remains, so
    /// backup status stays conservative after the mutation and after restart.
    pub fn finish_write(&self, name: &str) {
        let stamp = SystemTime::now();
        let seconds = stamp
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let Ok(_marker_guard) = self.marker_lock.lock() else {
            tracing::warn!(database = name, "backup write marker lock poisoned");
            if let Ok(mut writes) = self.writes.lock() {
                writes.remove(name);
            }
            return;
        };
        if self.persist_marker_value(name, seconds).is_ok() {
            if let Ok(mut writes) = self.writes.lock() {
                writes.insert(name.to_string(), stamp);
            }
        } else {
            tracing::warn!(
                database = name,
                "backup write marker persistence failed; freshness remains unknown"
            );
            if let Ok(mut writes) = self.writes.lock() {
                writes.remove(name);
            }
        }
    }

    /// Undo a `begin_write` fence for a mutation that rolled back without
    /// committing anything. The fence exists for crash-safety (die after the
    /// data commit but before the positive marker means `unknown`, never
    /// falsely fresh), but a rejected write changed nothing, so leaving the
    /// `-1` marker would flip a fresh database to `unknown` and fire a false
    /// Alert digest on the next poll. Callers capture `last_write` before
    /// `begin_write` and restore it here on the failure path.
    pub fn cancel_write(&self, name: &str, previous: Option<SystemTime>) {
        let Ok(_marker_guard) = self.marker_lock.lock() else {
            return;
        };
        if let Ok(conn) = self.marker_db.lock() {
            match previous {
                Some(stamp) => {
                    let seconds = stamp
                        .duration_since(UNIX_EPOCH)
                        .map(|duration| duration.as_secs() as i64)
                        .unwrap_or(i64::MAX);
                    let _ = conn.execute(
                        "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                         ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                        params![name, seconds],
                    );
                }
                None => {
                    let _ = conn.execute("DELETE FROM write_markers WHERE name=?", params![name]);
                }
            }
        }
        if let Ok(mut writes) = self.writes.lock() {
            match previous {
                Some(stamp) => {
                    writes.insert(name.to_string(), stamp);
                }
                None => {
                    writes.remove(name);
                }
            }
        }
    }

    /// Record a successful mutation for legacy store-level callers. New API
    /// mutations use `begin_write` before the write and `finish_write` after.
    #[cfg(test)]
    pub fn note_write_legacy(&self, name: &str) -> Result<()> {
        self.begin_write(name)?;
        self.finish_write(name);
        Ok(())
    }

    pub fn create_and_note(&self, name: &str) -> Result<()> {
        let previous = self.last_write(name);
        self.begin_write(name)?;
        let result = self.create(name);
        if result.is_ok() {
            self.finish_write(name);
        } else {
            self.cancel_write(name, previous);
        }
        result
    }

    pub fn last_write(&self, name: &str) -> Option<SystemTime> {
        let _marker_guard = self.marker_lock.lock().ok()?;
        self.writes
            .lock()
            .ok()
            .and_then(|writes| writes.get(name).copied())
    }

    fn persist_marker_value(&self, name: &str, seconds: i64) -> Result<()> {
        // Marker writes are tiny, but the same SQLite metadata file is also
        // used by authentication and lifecycle operations. Serialize our own
        // updates and retry transient busy/locked failures instead of merely
        // logging a warning after the data write has already committed.
        let conn = self.marker_db.lock().map_err(|_| Error::internal())?;
        let mut delay = Duration::from_millis(2);
        let mut last_error = Error::internal();
        for attempt in 0..5 {
            match conn.execute(
                "INSERT INTO write_markers(name,written_at) VALUES (?1,?2)
                 ON CONFLICT(name) DO UPDATE SET written_at=excluded.written_at",
                params![name, seconds],
            ) {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = error.into();
                    if attempt < 4 {
                        std::thread::sleep(delay);
                        delay = (delay * 2).min(Duration::from_millis(80));
                    }
                }
            }
        }
        Err(last_error)
    }

    pub fn backup_root(&self) -> Option<PathBuf> {
        self.backup_root.clone()
    }

    /// Versioned sets newest-first. None when no root is configured or the
    /// root is unreadable; unreadable individual sets are skipped, never
    /// presented as healthy.
    pub fn backup_sets(&self) -> Option<Vec<BackupSet>> {
        let root = self.backup_root.as_ref()?;
        let entries = std::fs::read_dir(root).ok()?;
        let mut sets = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_set_dir(&name) || !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            // The canonical producer writes lowercase `manifest.json`; the
            // uppercase name is accepted only as a compatibility reader for
            // older local sets. A missing or malformed member skips just this
            // set, never the whole scan.
            let manifest_path = ["manifest.json", "MANIFEST.json"]
                .into_iter()
                .map(|name| entry.path().join(name))
                .find(|path| path.is_file());
            let Some(manifest_path) = manifest_path else {
                continue;
            };
            let Some(bytes) = read_manifest_capped(&manifest_path) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let coverage_start = manifest["started_at"]
                .as_u64()
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
            let coverage_end = manifest["completed_at"]
                .as_u64()
                .and_then(|seconds| UNIX_EPOCH.checked_add(Duration::from_secs(seconds)));
            let databases = manifest["databases"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|e| e["name"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let registry_file = manifest["registry"]["file"].as_str();
            let has_registry = manifest["registry"]["sha256"].as_str().is_some()
                && registry_file.is_some_and(|file| {
                    !file.contains('/') && !file.contains('\\') && entry.path().join(file).is_file()
                });
            sets.push(BackupSet {
                timestamp: name,
                mtime,
                coverage_start,
                coverage_end,
                databases,
                has_registry,
            });
        }
        sets.sort_by_key(|set| std::cmp::Reverse(set.mtime));
        Some(sets)
    }
}
