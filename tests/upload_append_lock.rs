//! An upload append must not hold the transfer registry lock.
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

use dg::store::Store;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

    // The status needs the same lock the append used to hold.
    let start = Instant::now();
    let status = store.upload_status(&upload);
    let elapsed = start.elapsed();

    // Unblock the writer so the test does not leave a thread behind:
    // opening the FIFO for reading releases the writer's open.
    if let Ok(f) = std::fs::OpenOptions::new().read(true).open(&file) {
        drop(f);
    }
    let _ = writer.join();

    assert!(status.is_ok(), "the upload's status failed: {status:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "the status check waited {elapsed:?} behind the append's write"
    );
}
