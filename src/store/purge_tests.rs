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

#[test]
fn purge_prunes_per_name_state_and_write_markers() {
    // Per-name state is keyed by database name, so a create/purge cycle on
    // fresh names used to leave the queue guard, the usage counters, the
    // queue-wait counters, and the durable write marker behind forever --
    // three in-memory maps growing without bound over the process
    // lifetime, plus a metadata table that reloads every row at boot.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    store.bootstrap().unwrap();
    for index in 0..4 {
        let name = format!("cycle{index}");
        store.create(&name).unwrap();
        // Touch every per-name map the way a served request would.
        let _guard = store.acquire_queue(&name, false).unwrap();
        store.metrics.record_usage(&name, 10, 20);
        store
            .metrics
            .record_queue_wait(&name, Duration::from_millis(3), false);
        assert!(store.queues.lock().unwrap().contains_key(&name));
        assert!(store.metrics.usage_snapshot().contains_key(&name));
        assert!(store.metrics.queue_snapshot().contains_key(&name));
        let markers: i64 = store
            .meta()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM write_markers WHERE name=?",
                [&name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(markers, 1, "create must leave a freshness marker");

        store.delete(&name, 0).unwrap();
        assert_eq!(store.purge_expired().unwrap(), vec![name.clone()]);

        assert!(!store.queues.lock().unwrap().contains_key(&name));
        assert!(!store.metrics.usage_snapshot().contains_key(&name));
        assert!(!store.metrics.queue_snapshot().contains_key(&name));
        let markers: i64 = store
            .meta()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM write_markers WHERE name=?",
                [&name],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0, "purge must drop the freshness marker");
    }
    assert!(
        store.queues.lock().unwrap().is_empty(),
        "the queue map must not grow across create/purge cycles"
    );
}

/// A staging directory that cannot be removed must be reported, not
/// silently dropped.
///
/// The defect this pins: every transfer-cleanup path used
/// `let _ = std::fs::remove_dir_all(&dir)`, so a transient EACCES/EBUSY
/// (a stale NFS handle, a transient LSM lock, an antivirus scan of a
/// large upload) removed the in-memory transfer entry while leaving the
/// directory on disk — up to 128 MiB per transfer, `MAX_TRANSFERS` = 8 —
/// with no log line and no path back to the data. The directory became
/// unreachable until the orphan sweep at the next start, which counts
/// nothing and reports nothing until it runs.
///
/// The test builds exactly that state: a transfer whose parent directory
/// is made read-only so `remove_dir_all` fails, then asserts that (a) the
/// abort still succeeds — a failed cleanup must not fail the request that
/// asked for the removal, because the data is already unreachable — and
/// (b) the entry is gone from the map, so the invariant "the entry is
/// removed even when the directory is not" is explicit rather than
/// incidental, and the leak is at least reachable by the orphan sweep.
#[test]
fn unremovable_staging_dir_still_completes_the_abort() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let upload = store.upload_begin().unwrap();
    let transfer_dir = store
        .transfers
        .lock()
        .unwrap()
        .get(&upload.id)
        .unwrap()
        .dir
        .clone();
    // The stage lives directly in the data root, so making the root
    // read-only makes the stage unremovable while leaving the data root
    // itself writable enough for the rest of the store to work.
    use std::os::unix::fs::PermissionsExt as _;
    let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(mode & !0o222),
    )
    .unwrap();
    // With the parent unwritable the cleanup cannot succeed; the code
    // path under test must not let that fail the caller.
    let aborted = store.upload_abort(&upload.id);
    // Restore before asserting so the TempDir can still clean up.
    std::fs::set_permissions(
        dir.path(),
        std::os::unix::fs::PermissionsExt::from_mode(mode),
    )
    .unwrap();
    assert!(
        aborted.is_ok(),
        "an unremovable staging directory must not fail the abort: {:?}",
        aborted
    );
    assert!(
        transfer_dir.exists(),
        "the stage is expected to survive: the test made its parent unwritable"
    );
    // The in-memory entry is gone, which is what makes the orphan sweep
    // the (only) reclaim path — and what makes a silent failure a real
    // leak. Restoring the mode lets the sweep reclaim it.
    assert!(store.transfers.lock().unwrap().is_empty());
    let sweep_ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sweep_orphan_transfer_dirs(dir.path());
    }));
    assert!(sweep_ran.is_ok(), "the orphan sweep must stay runnable");
    assert!(
        !transfer_dir.exists(),
        "once the filesystem obstacle clears, the orphan sweep must reclaim the stage"
    );
}
