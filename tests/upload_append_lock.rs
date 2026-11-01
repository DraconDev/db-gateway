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
//! reader. While the append is parked there, a status check on ANOTHER
//! upload must still be answered -- it needs the same lock.

use dg::store::Store;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Find the staging file of an upload.
fn upload_file(root: &Path) -> Option<PathBuf> {
    // The staging directory is named after the transfer id's creation
    // time, not the id, so the file is found by its fixed name inside
    // the newest `.transfer-*` directory.
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !name.starts_with(".transfer-") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .collect();
    dirs.sort();
    let newest = dirs.pop()?;
    let file = newest.1.join("upload.bin");
    if file.exists() { Some(file) } else { None }
}

/// Replace the upload's file with a FIFO, so an append blocks on open.
fn make_it_a_fifo(path: &Path) -> bool {
    let _ = std::fs::remove_file(path);
    // mkfifo through libc's syscall interface: the standard library has
    // no named-pipe constructor.
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let result = unsafe { libc_mkfifo(c.as_ptr(), 0o600) };
    result == 0
}

unsafe extern "C" {
    #[link_name = "mkfifo"]
    fn libc_mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
}

/// A store over a temporary directory.
fn store(root: &Path) -> Arc<Store> {
    Arc::new(Store::open(root).unwrap())
}

/// The status of an upload is answered while another upload's append is
/// blocked in the middle of its write.
///
/// Before the fix, the append held the registry mutex across the file
/// open, so the FIFO's blocking open held the mutex too, and the status
/// check could not be answered until the append finished.
#[test]
fn an_upload_status_is_answered_while_another_append_is_writing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = store(&root);

    // Two uploads: one to append to, one to ask about.
    let writing = store.upload_begin().unwrap().id;
    let other = store.upload_begin().unwrap().id;

    // Park the first upload's append on a FIFO open.
    let file = upload_file(&root).expect("the upload staging file exists");
    assert!(
        make_it_a_fifo(&file),
        "the upload file could not become a FIFO"
    );

    let writer_store = store.clone();
    let writer = std::thread::spawn(move || writer_store.upload_append(&writing, 0, b"chunk"));

    // The append is now blocked in the open. Let it settle.
    std::thread::sleep(Duration::from_millis(300));

    // The status of the OTHER upload needs the same lock. It must be
    // answered promptly.
    let start = Instant::now();
    let status = store.upload_status(&other);
    let elapsed = start.elapsed();

    // Unblock the writer so the test does not leave a thread behind.
    // Opening the FIFO for reading releases the writer's open.
    if let Ok(file) = std::fs::OpenOptions::new().read(true).open(&file) {
        drop(file);
    }
    let _ = writer.join();

    assert!(
        status.is_ok(),
        "the other upload's status failed: {status:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the status check waited {elapsed:?} behind the append's write"
    );
}

/// The file permissions of the store do not change.
#[test]
fn the_store_root_is_readable_by_this_process() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(&root).unwrap();
    let store = store(&root);
    let _ = std::fs::metadata(&root).unwrap().permissions().mode();
    let upload = store.upload_begin().unwrap().id;
    assert!(store.upload_status(&upload).is_ok());
}
