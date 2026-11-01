use super::*;

/// Chunked-transfer bounds (large-database export/import). Chunks keep every
/// request and response small; the total ceiling is the documented maximum
/// database size for lifecycle transfer.
pub const MAX_TRANSFER_SIZE: u64 = 128 * 1024 * 1024;

pub const MAX_CHUNK: usize = 1024 * 1024;

pub const MAX_TRANSFERS: usize = 8;

pub const TRANSFER_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub(super) enum TransferKind {
    Snapshot { size: u64, sha256: String },
    Upload { received: u64, state: UploadState },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum UploadState {
    Open,
    Committing,
}

#[derive(Clone)]
pub(super) struct Transfer {
    pub(super) kind: TransferKind,
    pub(super) dir: PathBuf,
    file: PathBuf,
    /// Creator's credential hash. Snapshot chunks/release require the creator
    /// or the owner; uploads are owner-only at the API layer.
    creator: String,
    pub(super) created: Instant,
    /// Serializes the mutating operations of THIS transfer against each
    /// other: an upload's appends, its commit and its abort take turns.
    ///
    /// The registry mutex protects the registry, not the bytes. Holding it
    /// across a file write blocked every other transfer operation behind
    /// one 1 MiB write, so it was released before the write -- but the
    /// registry mutex never gave *per-upload* exclusion, and without it two
    /// appends for the same upload could both pass the offset check, both
    /// write, and then the loser's "the transfer moved on" branch truncated
    /// the file back to the old offset, deleting bytes the winner had
    /// already acknowledged. The acknowledged offset then no longer matched
    /// the bytes on disk and a chunked import silently lost data.
    ///
    /// One lock per transfer gives exactly that exclusion without holding
    /// the registry mutex during file I/O: appends, commits and aborts of
    /// one upload take turns, while every other transfer keeps running.
    /// Snapshot transfers never take it -- their file is frozen and only
    /// read.
    operation: Arc<Mutex<()>>,
}

/// Take a transfer's operation lock, recovering from a poisoned mutex.
///
/// A panic inside one append must not wedge the upload for its whole
/// lifetime: the guard is still handed out, because the state the lock
/// protects (the acknowledged offset and the file) is re-validated by
/// every operation that takes it.
fn lock_operation(operation: &Arc<Mutex<()>>) -> std::sync::MutexGuard<'_, ()> {
    operation
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct SnapshotInfo {
    pub id: String,
    pub size: u64,
    pub sha256: String,
}

pub struct UploadInfo {
    pub id: String,
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

fn snapshot_missing() -> Error {
    Error::new(404, "snapshot_not_found", "snapshot unknown or expired")
}

fn upload_missing() -> Error {
    Error::new(404, "upload_not_found", "upload unknown or expired")
}

fn is_transfer_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Best-effort removal of staging directories orphaned by a crash. A new
/// process owns no live transfers, so anything matching the staging prefixes
/// is residue. Failures are logged, never fatal to startup.
pub(super) fn sweep_orphan_transfer_dirs(root: &Path) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(error = %error, "transfer orphan sweep skipped");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_staging = name.starts_with(".transfer-") || name.starts_with(".lifecycle-");
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_staging
            && is_dir
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::warn!(path = %entry.path().display(), error = %error, "orphaned staging dir not removed");
        }
    }
}

/// Remove a transfer or lifecycle staging directory, recording a failure
/// instead of discarding it.
///
/// A silently swallowed `remove_dir_all` error is the defect this closes:
/// the caller has already removed the in-memory entry, so the directory is
/// unreachable by any later `remove` of the same id, and the orphan sweep
/// only runs at process start — so a single transient EACCES/EBUSY turned
/// into a permanent leak of up to 128 MiB per transfer, with `MAX_TRANSFERS`
/// counting only *live* entries and never noticing. Retrying is deliberately
/// not attempted here (the caller may hold a lock and the caller's own error
/// path must not be blocked by a filesystem retry), and the error is NOT
/// propagated: a failed cleanup of a directory whose data is already
/// unreachable must not fail the request that asked for the removal. What it
/// must not do is be invisible, so it is logged at warn with both the path
/// and the error, which is the same shape `sweep_orphan_transfer_dirs`
/// already uses for the same class of failure.
pub(super) fn remove_staging_dir(dir: &Path) {
    if let Err(error) = std::fs::remove_dir_all(dir) {
        tracing::warn!(
            path = %dir.display(),
            error = %error,
            "staging directory could not be removed; the orphan sweep at the next start reclaims it"
        );
    }
}

/// Streaming SHA-256 so hashing a large snapshot never loads it into memory.
fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|_| Error::internal())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(|_| Error::internal())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

impl Store {
    /// SQLite online backup of `name` to `dest`, consistent during writes.
    /// Pages are copied in bounded steps so concurrent commits interleave;
    /// the snapshot reflects one committed point in time.
    pub fn export_to(&self, name: &str, dest: &Path) -> Result<()> {
        // All lifecycle snapshots share the documented transfer ceiling. The
        // old unbounded call still gets the same protection as clone/export.
        self.export_to_limited(name, dest, Some(MAX_TRANSFER_SIZE))
    }

    /// SQLite online backup with an optional hard output ceiling. The check
    /// runs during backup steps so an over-limit snapshot is not fully
    /// materialized before the caller receives its 413. A `None` ceiling is
    /// treated as the lifecycle ceiling rather than as an unbounded escape.
    pub fn export_to_limited(&self, name: &str, dest: &Path, max_bytes: Option<u64>) -> Result<()> {
        const PAGES_PER_STEP: i32 = 64;
        const RETRY_STEPS: u64 = 64;
        const MAX_BUSY_RETRIES: u32 = 16;
        const RETRY_DELAY: Duration = Duration::from_millis(10);

        let src_path = self.path(name)?;
        // Atomically claim the destination: a check-then-open lets two
        // concurrent exports interleave on one path (or a swap land
        // between the check and SQLite's open). create_new makes the
        // loser a 409 deterministically.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
        {
            Ok(claimed) => drop(claimed),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Error::new(
                    409,
                    "file_exists",
                    "destination file already exists",
                ));
            }
            Err(_) => return Err(Error::internal()),
        }
        let limit = max_bytes.unwrap_or(MAX_TRANSFER_SIZE);
        // Deliberately no per-database write-slot acquisition: the SQLite
        // online backup API on a WAL source produces a consistent committed
        // snapshot while writes continue (the copy restarts if the source
        // changes between steps). Bound retries and derive the step budget from
        // the byte ceiling and the source's actual page size instead of
        // assuming the default 4096-byte SQLite page.
        let src =
            Connection::open_with_flags(&src_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        src.busy_timeout(Duration::from_millis(250))?;
        // A read transaction gives the online-backup source a stable page
        // count for this invocation. Individual steps still use SQLite's
        // online-backup protocol, so committed writers may proceed between
        // steps; the limit is enforced against the observed output as well as
        // the source page count.
        let page_size = src.query_row("PRAGMA page_size", [], |row| row.get::<_, i64>(0))?;
        let page_size = u64::try_from(page_size).map_err(|_| Error::internal())?;
        if !(512..=65536).contains(&page_size) {
            return Err(Error::internal());
        }
        let bytes_per_step = page_size.saturating_mul(PAGES_PER_STEP as u64).max(1);
        let max_steps = limit
            .saturating_add(bytes_per_step - 1)
            .checked_div(bytes_per_step)
            .unwrap_or(0)
            .saturating_add(RETRY_STEPS);
        let mut dst = Connection::open(dest)?;
        dst.busy_timeout(Duration::from_millis(250))?;

        let outcome = (|| -> Result<()> {
            let backup = Backup::new(&src, &mut dst)?;
            let mut result = StepResult::More;
            let mut steps = 0u64;
            let mut busy_retries = 0u32;
            let mut failure = None;
            while matches!(
                result,
                StepResult::More | StepResult::Busy | StepResult::Locked
            ) {
                if steps >= max_steps {
                    failure = Some(Error::new(
                        429,
                        "capacity_exceeded",
                        "backup could not complete within bounded transfer work",
                    ));
                    break;
                }
                steps += 1;
                result = match backup.step(PAGES_PER_STEP) {
                    Ok(result) => result,
                    Err(error) => {
                        failure = Some(error.into());
                        break;
                    }
                };
                if matches!(result, StepResult::Busy | StepResult::Locked) {
                    if busy_retries >= MAX_BUSY_RETRIES {
                        failure = Some(Error::new(
                            429,
                            "capacity_exceeded",
                            "backup could not acquire a consistent source snapshot",
                        ));
                        break;
                    }
                    busy_retries += 1;
                    thread::sleep(RETRY_DELAY);
                    continue;
                }
                busy_retries = 0;
                let progress = backup.progress();
                let expected = (progress.pagecount.max(0) as u64).saturating_mul(page_size);
                let observed = std::fs::metadata(dest)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0);
                if expected > limit || observed > limit {
                    failure = Some(Error::new(
                        413,
                        "result_limit_exceeded",
                        "export exceeds bounded response size",
                    ));
                    break;
                }
            }
            drop(backup);
            if let Some(error) = failure {
                return Err(error);
            }
            match result {
                StepResult::Done => {
                    // The final step can be the first one to cross the limit.
                    if std::fs::metadata(dest)
                        .map(|metadata| metadata.len() > limit)
                        .unwrap_or(false)
                    {
                        return Err(Error::new(
                            413,
                            "result_limit_exceeded",
                            "export exceeds bounded response size",
                        ));
                    }
                    dst.pragma_update(None, "journal_mode", "DELETE")?;
                    Ok(())
                }
                _ => Err(Error::new(
                    429,
                    "capacity_exceeded",
                    "backup could not complete; source busy",
                )),
            }
        })();
        if outcome.is_err() {
            drop(dst);
            remove_database_files(dest);
        }
        outcome
    }

    pub(super) fn staging(&self) -> Result<Staging> {
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
        self.export_snapshot_limited(name, None)
    }

    pub fn export_snapshot_limited(&self, name: &str, max_bytes: Option<u64>) -> Result<Staging> {
        let temp = self.staging()?;
        self.export_to_limited(name, temp.path(), max_bytes)?;
        Ok(temp)
    }

    pub fn import_staging(&self, name: &str) -> Result<Staging> {
        validate_name(name)?;
        self.staging()
    }

    /// Take a retained online-backup snapshot for chunked download. The
    /// snapshot is a consistent committed point in time; chunks served later
    /// read this frozen copy, so concurrent writes never tear a transfer.
    pub fn export_begin(&self, principal: &Principal, name: &str) -> Result<SnapshotInfo> {
        let dir = self.retained_transfer_dir()?;
        let file = dir.join("snapshot.sqlite");
        let outcome = (|| {
            self.export_to_limited(name, &file, Some(MAX_TRANSFER_SIZE))?;
            let size = std::fs::metadata(&file)
                .map_err(|_| Error::internal())?
                .len();
            if size > MAX_TRANSFER_SIZE {
                return Err(Error::new(
                    413,
                    "result_limit_exceeded",
                    "database exceeds chunked transfer ceiling",
                ));
            }
            let sha256 = sha256_file(&file)?;
            let id = generate_key();
            self.insert_transfer(
                id.clone(),
                Transfer {
                    kind: TransferKind::Snapshot {
                        size,
                        sha256: sha256.clone(),
                    },
                    dir: dir.clone(),
                    file,
                    creator: principal.hash.clone(),
                    created: Instant::now(),
                    operation: Arc::new(Mutex::new(())),
                },
            )?;
            Ok(SnapshotInfo { id, size, sha256 })
        })();
        if outcome.is_err() {
            remove_staging_dir(&dir);
        }
        outcome
    }

    /// Read one chunk of a snapshot. Unknown, expired, wrong-kind, and
    /// foreign-principal ids all report 404 so ids are not an oracle.
    pub fn snapshot_chunk(
        &self,
        principal: &Principal,
        id: &str,
        offset: u64,
        length: usize,
    ) -> Result<(Vec<u8>, u64, String)> {
        // Lookup under the lock, clone what the read needs, then release
        // before touching the filesystem: holding the registry mutex across
        // file open/seek/read serialized every transfer operation (appends,
        // status, aborts, other chunks) behind one slow 1 MiB read. The
        // post-read revalidation below closes the lookup-vs-read race the
        // coarse lock used to cover: a concurrent release (or TTL purge)
        // between lookup and open reports an honest 404 instead of a 500.
        let (file_path, take, size, sha256) = {
            let transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            let transfer = match transfers.get(id) {
                Some(entry) if entry.created.elapsed() < self.transfer_ttl => entry.clone(),
                Some(_) => return Err(snapshot_missing()),
                None => return Err(snapshot_missing()),
            };
            if transfer.creator != principal.hash && principal.role != "owner" {
                return Err(snapshot_missing());
            }
            let (size, sha256) = match &transfer.kind {
                TransferKind::Snapshot { size, sha256 } => (*size, sha256.clone()),
                TransferKind::Upload { .. } => return Err(snapshot_missing()),
            };
            if offset > size {
                return Err(Error::invalid());
            }
            if length == 0 {
                return Err(Error::invalid());
            }
            let take = (length as u64).min(MAX_CHUNK as u64).min(size - offset) as usize;
            (transfer.file.clone(), take, size, sha256)
        };
        // Stale-entry directory removal happens under the lock in the
        // callers that own expiry; here an expired-or-released transfer
        // simply reports 404 without touching the filesystem.
        let mut file = match std::fs::File::open(&file_path) {
            Ok(file) => file,
            Err(_) => {
                // A concurrent release removes the retained directory
                // between lookup and open: report 404 when the transfer is
                // gone, 500 only when it is still registered.
                let gone = self
                    .transfers
                    .lock()
                    .map(|transfers| transfers.get(id).is_none())
                    .unwrap_or(false);
                return Err(if gone {
                    snapshot_missing()
                } else {
                    Error::internal()
                });
            }
        };
        use std::io::{Read, Seek};
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(|_| Error::internal())?;
        let mut bytes = vec![0u8; take];
        file.read_exact(&mut bytes).map_err(|_| Error::internal())?;
        // Revalidate: the transfer must still be registered, unexpired,
        // and unchanged after the read, or the bytes may come from a
        // released directory and must not be served.
        {
            let transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            match transfers.get(id) {
                Some(entry)
                    if entry.created.elapsed() < self.transfer_ttl
                        && entry.file == file_path
                        && (entry.creator == principal.hash || principal.role == "owner") =>
                {
                    match &entry.kind {
                        TransferKind::Snapshot {
                            size: live_size,
                            sha256: live_sha,
                        } if *live_size == size && *live_sha == sha256 => {}
                        _ => return Err(snapshot_missing()),
                    }
                }
                _ => return Err(snapshot_missing()),
            }
        }
        Ok((bytes, size, sha256))
    }

    pub fn snapshot_release(&self, principal: &Principal, id: &str) -> Result<()> {
        let transfer = self.lookup_transfer(id)?.ok_or_else(snapshot_missing)?;
        if transfer.creator != principal.hash && principal.role != "owner" {
            return Err(snapshot_missing());
        }
        if !matches!(transfer.kind, TransferKind::Snapshot { .. }) {
            return Err(snapshot_missing());
        }
        self.remove_transfer(id);
        Ok(())
    }

    /// Open a retained upload for chunked import. Owner-only at the API layer.
    pub fn upload_begin(&self) -> Result<UploadInfo> {
        let dir = self.retained_transfer_dir()?;
        let file = dir.join("upload.bin");
        std::fs::write(&file, []).map_err(|_| Error::internal())?;
        let id = generate_key();
        let outcome = self.insert_transfer(
            id.clone(),
            Transfer {
                kind: TransferKind::Upload {
                    received: 0,
                    state: UploadState::Open,
                },
                dir: dir.clone(),
                file,
                creator: String::new(),
                created: Instant::now(),
                operation: Arc::new(Mutex::new(())),
            },
        );
        if outcome.is_err() {
            remove_staging_dir(&dir);
        }
        outcome?;
        Ok(UploadInfo { id })
    }

    /// Append one chunk at exactly the current received length. Gaps and
    /// overwrites are rejected; resume by reading status and continuing.
    pub fn upload_append(&self, id: &str, offset: u64, bytes: &[u8]) -> Result<u64> {
        if !is_transfer_id(id) {
            return Err(upload_missing());
        }
        if bytes.len() > MAX_CHUNK {
            return Err(Error::invalid());
        }
        // Serialize this upload's own operations against each other. The
        // registry mutex is still only held for the lookups below, so the
        // file write never blocks another transfer's status check, chunk or
        // abort -- but two appends of THIS upload can no longer both pass
        // the offset check and write over each other. The lock is taken
        // before the offset is read, so the offset and the bytes it
        // describes belong to the same turn.
        let operation = self.upload_operation(id)?;
        let _guard = lock_operation(&operation);

        // Re-read the acknowledged offset under the registry lock: another
        // append of this upload may have advanced it while this one waited
        // for the lock, and a commit may have claimed the upload.
        let (file_path, prior) = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            let transfer = match transfers.get_mut(id) {
                Some(t) if t.created.elapsed() < self.transfer_ttl => t,
                _ => return Err(upload_missing()),
            };
            let TransferKind::Upload { received, state } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if *state != UploadState::Open {
                return Err(Error::busy());
            }
            if offset != *received {
                return Err(Error::invalid());
            }
            if *received + bytes.len() as u64 > MAX_TRANSFER_SIZE {
                return Err(Error::new(
                    413,
                    "result_limit_exceeded",
                    "upload exceeds chunked transfer ceiling",
                ));
            }
            (transfer.file.clone(), *received)
        };
        use std::io::Write;
        let write = std::fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .and_then(|mut f| {
                // The file must hold exactly the acknowledged bytes: the
                // append writes at the end, so a tail that outlived its
                // acknowledgement would be kept and the file would no
                // longer match the offset it is asked about.
                f.set_len(prior)?;
                f.write_all(bytes).inspect_err(|_error| {
                    // write_all may persist a prefix before returning an I/O
                    // error. Restore the acknowledged offset so retrying appends
                    // the complete chunk exactly once.
                    let _ = f.set_len(prior);
                })?;
                // The file ends exactly at the offset this append is about
                // to acknowledge.
                f.set_len(prior + bytes.len() as u64)
            });
        if let Err(error) = write {
            tracing::warn!(error = %error, "upload append failed");
            // An upload that was aborted or expired while the bytes were
            // being written is gone, not broken.
            let gone = self
                .transfers
                .lock()
                .map(|transfers| transfers.get(id).is_none())
                .unwrap_or(false);
            return Err(if gone {
                upload_missing()
            } else {
                Error::internal()
            });
        }
        // Advance the acknowledged offset under the lock. Only another
        // append moves it, and the appends of one upload are serialized by
        // the operation lock held above, so it cannot have moved under this
        // write. The state is deliberately NOT re-checked here: a commit
        // that claimed the upload while this chunk was being written is
        // waiting for exactly this append to finish, and its import must
        // include the bytes this append is about to acknowledge.
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        let Some(transfer) = transfers.get_mut(id) else {
            return Err(upload_missing());
        };
        let TransferKind::Upload { received, .. } = &mut transfer.kind else {
            return Err(upload_missing());
        };
        if *received != prior {
            // Unreachable while the operation lock is held. The bytes are
            // NOT trimmed back: truncating a file whose length another
            // request already acknowledged is the data loss this lock
            // exists to prevent, so the inconsistency is reported instead.
            tracing::warn!(
                upload = id,
                acknowledged = *received,
                expected = prior,
                "the acknowledged offset moved while the chunk was written"
            );
            return Err(Error::internal());
        }
        *received += bytes.len() as u64;
        Ok(*received)
    }

    pub fn upload_status(&self, id: &str) -> Result<u64> {
        match self.lookup_transfer(id)? {
            Some(t) => match t.kind {
                TransferKind::Upload { received, .. } => Ok(received),
                TransferKind::Snapshot { .. } => Err(upload_missing()),
            },
            None => Err(upload_missing()),
        }
    }

    /// Validate and publish an upload under `name`, reusing the atomic
    /// single-shot import path. A failed commit keeps the upload so the
    /// client can fix the target name and retry; abort or TTL releases it.
    pub fn upload_commit(&self, id: &str, name: &str) -> Result<()> {
        // Claim the upload for the commit under the registry lock, so a
        // second commit and every new append are rejected while the
        // publication runs.
        let (file, operation) = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            self.purge_expired_locked(&mut transfers);
            let transfer = transfers.get_mut(id).ok_or_else(upload_missing)?;
            let TransferKind::Upload { state, .. } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if transfer.created.elapsed() >= self.transfer_ttl {
                return Err(upload_missing());
            }
            if *state != UploadState::Open {
                return Err(Error::busy());
            }
            *state = UploadState::Committing;
            (transfer.file.clone(), transfer.operation.clone())
        };
        // Wait for an in-flight append of this upload to finish before the
        // file is read: a commit that imports the file while a chunk is
        // still being written publishes a half-written database. The
        // registry mutex is NOT held here, so every other transfer
        // operation keeps running.
        let _guard = lock_operation(&operation);
        // The upload may have been aborted or expired while the commit
        // waited for the append to finish.
        let received = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            let transfer = transfers.get_mut(id).ok_or_else(upload_missing)?;
            let TransferKind::Upload { received, state } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if *state != UploadState::Committing {
                return Err(Error::busy());
            }
            *received
        };
        // The file must hold exactly the acknowledged bytes. A file that is
        // longer or shorter than the acknowledged offset is not the upload
        // the client assembled, and importing it would publish something
        // the client never sent.
        let length = std::fs::metadata(&file)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if length != received {
            tracing::warn!(
                upload = id,
                on_disk = length,
                acknowledged = received,
                "the upload file and its acknowledged offset disagree"
            );
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            self.restore_open_after_failed_commit(id, &mut transfers);
            return Err(Error::new(
                409,
                "upload_out_of_sync",
                "the upload file does not match its acknowledged length",
            ));
        }

        let result = self.import_from(name, &file);
        if result.is_ok() {
            self.remove_transfer(id);
            return Ok(());
        }

        // A rejected target name or validation failure leaves the complete
        // upload available for a corrected retry, but never concurrent with
        // another append/abort while publication was in progress.
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        self.restore_open_after_failed_commit(id, &mut transfers);
        result
    }

    pub fn upload_abort(&self, id: &str) -> Result<()> {
        // The abort waits for an in-flight append of this upload, so the
        // upload is either aborted as a whole or not at all -- never
        // half-written with its acknowledged offset pointing past the end.
        let operation = self.upload_operation(id)?;
        let _guard = lock_operation(&operation);
        let removed = {
            let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
            self.purge_expired_locked(&mut transfers);
            let transfer = transfers.get_mut(id).ok_or_else(upload_missing)?;
            let TransferKind::Upload { state, .. } = &mut transfer.kind else {
                return Err(upload_missing());
            };
            if *state != UploadState::Open {
                return Err(Error::busy());
            }
            transfers.remove(id)
        };
        if let Some(transfer) = removed {
            remove_staging_dir(&transfer.dir);
        }
        Ok(())
    }

    /// The operation lock of the upload `id`, purging an expired entry.
    ///
    /// The lookup takes the registry mutex only for the lookup itself; the
    /// lock it returns is the upload's own, so the caller can serialize
    /// against the upload's other operations without holding the registry
    /// mutex across any file I/O.
    fn upload_operation(&self, id: &str) -> Result<Arc<Mutex<()>>> {
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        match transfers.get(id) {
            Some(transfer) if transfer.created.elapsed() < self.transfer_ttl => {
                Ok(transfer.operation.clone())
            }
            Some(_) => {
                let stale = transfers.remove(id);
                drop(transfers);
                if let Some(transfer) = stale {
                    remove_staging_dir(&transfer.dir);
                }
                Err(upload_missing())
            }
            None => Err(upload_missing()),
        }
    }

    /// Drop expired transfers and their retained directories. Driven by the
    /// server maintenance loop; begin paths purge inline before counting.
    pub fn sweep_transfers(&self) -> Result<usize> {
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        Ok(self.purge_expired_locked(&mut transfers))
    }

    fn retained_transfer_dir(&self) -> Result<PathBuf> {
        tempfile::Builder::new()
            .prefix(".transfer-")
            .tempdir_in(&self.root)
            .map_err(|_| Error::internal())
            .map(|dir| dir.keep())
    }

    fn insert_transfer(&self, id: String, transfer: Transfer) -> Result<()> {
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        self.purge_expired_locked(&mut transfers);
        if transfers.len() >= MAX_TRANSFERS {
            return Err(Error::busy());
        }
        transfers.insert(id, transfer);
        Ok(())
    }

    fn lookup_transfer(&self, id: &str) -> Result<Option<Transfer>> {
        if !is_transfer_id(id) {
            return Ok(None);
        }
        let mut transfers = self.transfers.lock().map_err(|_| Error::internal())?;
        match transfers.get(id) {
            Some(t) if t.created.elapsed() < self.transfer_ttl => Ok(Some(t.clone())),
            Some(_) => {
                let stale = transfers.remove(id);
                drop(transfers);
                if let Some(t) = stale {
                    remove_staging_dir(&t.dir);
                }
                Ok(None)
            }
            None => Ok(None),
        }
    }

    /// Restore a `Committing` upload to `Open` after a failed publication,
    /// so the client can retry with a corrected target name. An upload whose
    /// TTL elapsed while publication ran is NOT revived: the entry is
    /// already gone (purged below) and reviving it would make a dead
    /// upload look resumable until the next append/abort re-checks the TTL
    /// and 404s. A rejected name is recoverable; an expired upload is not,
    /// and the two must not look alike.
    pub(super) fn restore_open_after_failed_commit(
        &self,
        id: &str,
        transfers: &mut HashMap<String, Transfer>,
    ) {
        self.purge_expired_locked(transfers);
        if let Some(transfer) = transfers.get_mut(id)
            && let TransferKind::Upload { state, .. } = &mut transfer.kind
            && transfer.created.elapsed() < self.transfer_ttl
        {
            *state = UploadState::Open;
        }
    }

    fn remove_transfer(&self, id: &str) {
        let removed = self
            .transfers
            .lock()
            .map(|mut map| map.remove(id))
            .ok()
            .flatten();
        if let Some(t) = removed {
            remove_staging_dir(&t.dir);
        }
    }
}
