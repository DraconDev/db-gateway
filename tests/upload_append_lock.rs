//! An upload append must not hold the transfer registry lock, and the
//! uploads' own operations must not run over each other.
//!
//! Every transfer operation -- another chunk, a status check, an abort,
//! a snapshot read -- takes the same registry mutex. When an append
//! held that mutex across the file open and the write, all of them
//! queued behind one disk write of up to 1 MiB. The sibling
//! `snapshot_chunk` had already been changed to look the transfer up
//! under the lock, clone what it needs, and release the lock before
//! touching the filesystem; the upload append had not.
//!
//! The test makes the write block deterministically: the upload's file
//! is replaced with a FIFO, so opening it for append waits for a
//! reader. While the append is parked there, a status check on the
//! upload must still be answered -- it needs the same lock.
//!
//! Releasing the registry mutex is not the same as releasing the upload:
//! with nothing serializing one upload's own operations, two appends at
//! the same offset both passed the offset check and both wrote, and the
//! loser's "the transfer moved on" branch truncated the file back to the
//! old offset -- deleting bytes the winner had already acknowledged. The
//! offset the client was told about then no longer described the file.
//! Each upload therefore has its own operation lock: the appends, the
//! commit and the abort of one upload take turns, while every other
//! transfer keeps running.

use dg::store::Store;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

unsafe extern "C" {
    #[link_name = "mkfifo"]
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
}

/// The staging file of the one upload this store holds.
///
/// The staging directory is named after the transfer, not the id, so the
/// file is found by its fixed name inside the store's only `.transfer-*`
/// directory.
fn upload_file(root: &Path) -> Option<PathBuf> {
    let mut found = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with(".transfer-") {
                return None;
            }
            let file = path.join("upload.bin");
            if file.exists() { Some(file) } else { None }
        })
        .collect::<Vec<_>>();
    match found.len() {
        1 => found.pop(),
        _ => None,
    }
}

/// Replace the upload's file with a FIFO, so an append blocks on open.
fn make_it_a_fifo(path: &Path) -> bool {
    let _ = std::fs::remove_file(path);
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // SAFETY: the path is NUL-terminated and owned by the CString.
    unsafe { mkfifo(c.as_ptr(), 0o600) == 0 }
}

/// A store over a temporary directory.
fn store(root: &Path) -> Arc<Store> {
    Arc::new(Store::open(root).unwrap())
}

/// The upload's status is answered while its append is blocked in the
/// middle of the write.
///
/// Before the fix, the append held the registry mutex across the file
/// open, so the FIFO's blocking open held the mutex too, and the status
/// check could not be answered until the append finished.
#[test]
fn an_upload_status_is_answered_while_its_append_is_writing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = store(&root);

    let upload = store.upload_begin().unwrap().id;

    // Park the append on a FIFO open.
    let file = upload_file(&root).expect("the upload staging file exists");
    assert!(
        make_it_a_fifo(&file),
        "the upload file could not become a FIFO"
    );

    let writer_store = store.clone();
    let writer_id = upload.clone();
    let writer = std::thread::spawn(move || writer_store.upload_append(&writer_id, 0, b"chunk"));

    // The append is now blocked in the open. Let it settle.
    std::thread::sleep(Duration::from_millis(300));

    // The status needs the same lock the append used to hold. It runs
    // on its own thread with a deadline, so a lock that is never
    // released is a FAILURE and not a hung test.
    let (sender, receiver) = std::sync::mpsc::channel();
    let status_store = store.clone();
    let status_id = upload.clone();
    let start = Instant::now();
    std::thread::spawn(move || {
        let answer = status_store.upload_status(&status_id);
        let _ = sender.send((answer, start.elapsed()));
    });

    let answered = receiver.recv_timeout(Duration::from_secs(5));

    // Unblock the writer so the test does not leave a thread behind:
    // opening the FIFO for reading releases the writer's open.
    if let Ok(f) = std::fs::OpenOptions::new().read(true).open(&file) {
        drop(f);
    }
    let _ = writer.join();

    let (status, elapsed) = answered.expect(
        "the status check never came back: the append held the transfer registry lock across its write",
    );
    assert!(status.is_ok(), "the upload's status failed: {status:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the status check waited {elapsed:?} behind the append's write"
    );
}

/// The bytes an upload has acknowledged must be exactly the bytes on disk,
/// even when several appends of the same upload race for the same offset.
///
/// Before the fix, two appends at the same offset both passed the offset
/// check: each read the acknowledged offset under the registry lock,
/// released it, and wrote. The loser's "the transfer moved on" branch then
/// truncated the file back to the old offset, deleting bytes the winner had
/// already acknowledged, and the offset the client was told about stopped
/// describing the file. A chunked import could lose data with no error
/// reaching the client.
///
/// The upload's own operation lock is what makes the offset and the bytes
/// belong to the same turn: the appends take turns, so exactly one of them
/// is acknowledged and the file ends up holding exactly that chunk.
#[test]
fn two_appends_at_the_same_offset_leave_the_acknowledged_offset_matching_the_bytes_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root).unwrap());
    let upload = store.upload_begin().unwrap().id;

    let chunk = 64 * 1024;
    // A barrier, so the appends really do race for the same offset.
    let barrier = Arc::new(Barrier::new(4));
    let mut threads = Vec::new();
    for tag in *b"ABCD" {
        let store = store.clone();
        let upload = upload.clone();
        let barrier = barrier.clone();
        threads.push(std::thread::spawn(move || {
            let bytes = vec![tag; chunk];
            barrier.wait();
            (tag, store.upload_append(&upload, 0, &bytes))
        }));
    }

    let mut acknowledged: Option<(u8, u64)> = None;
    let mut rejected = 0;
    for thread in threads {
        let (tag, outcome) = thread.join().unwrap();
        match outcome {
            Ok(offset) => {
                assert!(
                    acknowledged.is_none(),
                    "two appends were acknowledged at the same offset: {acknowledged:?} and {offset}"
                );
                acknowledged = Some((tag, offset));
            }
            Err(_) => rejected += 1,
        }
    }
    let (tag, offset) = acknowledged.expect("no append was acknowledged");
    assert_eq!(
        rejected, 3,
        "the losing appends must be rejected on their offset, not acknowledged"
    );
    assert_eq!(
        offset, chunk as u64,
        "the acknowledged offset is not the length of the chunk"
    );

    // The acknowledged offset must describe the bytes on disk.
    let file = upload_file(&root).expect("the upload staging file exists");
    let on_disk = std::fs::read(&file).unwrap();
    assert_eq!(
        on_disk.len() as u64,
        offset,
        "the file does not hold as many bytes as the acknowledged offset"
    );
    assert!(
        on_disk.iter().all(|byte| *byte == tag),
        "the file holds bytes that were never acknowledged together"
    );
    assert_eq!(
        store.upload_status(&upload).unwrap(),
        offset,
        "the reported status does not match the acknowledged offset"
    );
}

/// A commit must not read the upload file while a chunk is still being
/// written into it.
///
/// The commit publishes the uploaded file. Before the fix it did so
/// without waiting for an in-flight append, so a commit that arrived while
/// a chunk was being written could publish a half-written database. The
/// commit now waits for the upload's own append to finish, and it checks
/// that the file holds exactly the acknowledged bytes before it reads it.
#[test]
fn a_commit_waits_for_an_in_flight_append_before_it_reads_the_upload_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(&root).unwrap());
    let upload = store.upload_begin().unwrap().id;

    // The upload's file becomes a FIFO whose reader never drains it: a
    // chunk larger than the pipe's buffer blocks in the write, so the
    // append stays in flight for as long as the test needs it to.
    let file = upload_file(&root).expect("the upload staging file exists");
    assert!(
        make_it_a_fifo(&file),
        "the upload file could not become a FIFO"
    );

    let chunk = vec![b'A'; 1024 * 1024];
    let payload = chunk.clone();
    let append_store = store.clone();
    let append_id = upload.clone();
    let append = std::thread::spawn(move || append_store.upload_append(&append_id, 0, &payload));

    // A FIFO's read open waits for a writer, so it is opened after the
    // append is on its way: the append's write open completes with it,
    // and the append then blocks in the write because the pipe is full
    // and nobody drains it.
    let mut reader = std::fs::OpenOptions::new().read(true).open(&file).unwrap();
    // A writer as well, so that a commit which reaches the file does not
    // block on the FIFO's own open.
    let mut writer = std::fs::OpenOptions::new().write(true).open(&file).unwrap();

    // Let the append reach the blocked write.
    std::thread::sleep(Duration::from_millis(300));

    let commit_store = store.clone();
    let commit_id = upload.clone();
    let (sender, receiver) = std::sync::mpsc::channel();
    let commit = std::thread::spawn(move || {
        let outcome = commit_store.upload_commit(&commit_id, "raced.sqlite");
        let _ = sender.send(outcome);
    });

    std::thread::sleep(Duration::from_millis(500));

    // The commit must not have read the file: the append is still writing.
    assert!(
        receiver.try_recv().is_err(),
        "the commit published the upload while a chunk was still being written"
    );
    // A second commit is refused while the first one is publishing.
    assert!(
        store.upload_commit(&upload, "probe.sqlite").is_err(),
        "a second commit ran while the first one was publishing"
    );

    // Release the blocked append by draining the pipe.
    let mut drained = 0;
    let mut buffer = [0u8; 64 * 1024];
    while drained < chunk.len() {
        let read = reader.read(&mut buffer).unwrap();
        assert!(read > 0, "the FIFO reader hit end of file early");
        drained += read;
    }

    // The commit reads the FIFO once the append is done; give it a header
    // it can reject.
    writer.write_all(&[0u8; 128]).unwrap();
    writer.flush().unwrap();

    let outcome = receiver
        .recv_timeout(Duration::from_secs(30))
        .expect("the commit never returned");
    let _ = commit.join();
    let _ = append.join();

    assert!(
        outcome.is_err(),
        "the commit published a file that is not a database: {outcome:?}"
    );
    assert!(
        !root.join("raced.sqlite").exists(),
        "the commit published a database built from a half-written upload"
    );
}

/// An upload left with unacknowledged bytes on disk is repaired, not
/// bricked.
///
/// The append writes the chunk and only then advances the acknowledged
/// offset. A write error whose rollback also fails leaves the file longer
/// than the offset, and the first version of this fix answered that with a
/// 409 that nothing ever repaired: every later append and every commit
/// repeated the 409 and the client's only way out was to abort and start
/// the whole upload again.
///
/// The bytes past the acknowledged offset were never acknowledged, and the
/// transfer's operation lock means no append is running, so trimming them
/// is exactly the repair -- and unlike the truncation this lock replaced,
/// it cannot cut bytes another request already acknowledged.
#[test]
fn unacknowledged_bytes_are_trimmed_instead_of_bricking_the_upload() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).unwrap();
    let upload = store.upload_begin().unwrap().id;

    store.upload_append(&upload, 0, b"acknowledged").unwrap();
    let acknowledged = store.upload_status(&upload).unwrap();
    assert_eq!(acknowledged, b"acknowledged".len() as u64);

    // Simulate the unrepaired tail of a failed write.
    let file = upload_file(&root).expect("the upload staging file exists");
    {
        use std::io::Write;
        let mut handle = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        handle.write_all(b"never-acknowledged").unwrap();
    }
    assert!(
        std::fs::metadata(&file).unwrap().len() > acknowledged,
        "the test did not actually leave a tail on disk"
    );

    // The next chunk at the acknowledged offset is accepted, and the file
    // ends up holding exactly what was acknowledged -- the tail is gone,
    // the acknowledged bytes are intact.
    let received = store
        .upload_append(&upload, acknowledged, b"-tail")
        .unwrap();
    let expected: Vec<u8> = b"acknowledged-tail".to_vec();
    assert_eq!(received, expected.len() as u64);
    assert_eq!(store.upload_status(&upload).unwrap(), received);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        expected,
        "the repaired file does not hold exactly the acknowledged bytes"
    );
}

/// A file that holds FEWER bytes than were acknowledged is refused, not
/// quietly accepted: importing it would publish a database missing chunks
/// the client was told were received.
#[test]
fn a_file_shorter_than_its_acknowledged_offset_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).unwrap();
    let upload = store.upload_begin().unwrap().id;

    store
        .upload_append(&upload, 0, b"two-chunks-worth")
        .unwrap();
    let file = upload_file(&root).expect("the upload staging file exists");
    // Lose the tail without losing the acknowledgement.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(4)
        .unwrap();

    let error = store
        .upload_append(&upload, b"two-chunks-worth".len() as u64, b"x")
        .unwrap_err();
    assert_eq!(error.code, "upload_out_of_sync");
    assert_eq!(
        std::fs::metadata(&file).unwrap().len(),
        4,
        "a refused upload must not be silently rewritten"
    );
    let commit = store.upload_commit(&upload, "short.sqlite").unwrap_err();
    assert_eq!(commit.code, "upload_out_of_sync");
    assert!(
        !root.join("short.sqlite").exists(),
        "an upload missing acknowledged bytes was published"
    );
}
