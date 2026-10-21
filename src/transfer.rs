//! Snapshot export and import: the chunked transfers, in one place.
//!
//! An export and an import are the same operation split into steps:
//! begin, move the bytes, then commit or publish. The CLI and the
//! terminal view both perform them, so the steps live here — one
//! implementation of the hash check, the byte count and the atomic
//! publication, rather than two that can disagree about them.
//!
//! A transfer that fails halfway leaves nothing behind: the download
//! is staged and published only when it is complete and its hash
//! matches, and the upload is aborted when the commit does not land.

use crate::{
    client::{self, Semantics},
    error::{Error, Result},
    store,
};
use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
};

/// A transfer that failed.
///
/// The gateway's envelope travels with the failure when the gateway is
/// what refused: the CLI prints that envelope exactly as it always
/// has, and the terminal view shows the code and the message.
pub struct Failure {
    /// The error the command returns.
    pub error: Error,
    /// The gateway's error envelope, when the gateway refused.
    pub envelope: Option<Value>,
}

impl Failure {
    /// A local failure: nothing was sent, or nothing came back.
    fn local(error: Error) -> Self {
        Self {
            error,
            envelope: None,
        }
    }

    /// A refusal from the gateway.
    fn refused(envelope: Value) -> Self {
        Self {
            error: Error::new(400, "api_error", "API request failed; see JSON response"),
            envelope: Some(envelope),
        }
    }
}

/// What an export produced.
#[derive(Clone, Debug)]
pub struct ExportReport {
    /// The database that was exported.
    pub name: String,
    /// Where the file was published.
    pub out: PathBuf,
    /// How many bytes the file holds.
    pub size: u64,
    /// The hash of the whole file, so the operator can check it.
    pub sha256: String,
}

/// What an import produced.
#[derive(Clone, Debug)]
pub struct ImportReport {
    /// The database the file became.
    pub name: String,
    /// The file that was read.
    pub from: PathBuf,
    /// How many bytes were uploaded.
    pub size: u64,
    /// The gateway's answer to the commit.
    pub response: Value,
}

/// Whether the gateway refused.
fn api_failed(response: &Value) -> bool {
    response.get("error").is_some()
}

/// Stage an export download next to its destination so publication
/// below is a same-directory atomic no-clobber link. Chunks stream in
/// one at a time; only a single chunk is ever buffered in memory.
fn stage_file(out: &Path) -> Result<tempfile::NamedTempFile> {
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    tempfile::NamedTempFile::new_in(parent).map_err(|_| Error::internal())
}

/// Publish a staged file at its destination.
fn publish_file(staged: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    staged.as_file().sync_all().map_err(|_| Error::internal())?;
    // A same-directory hard link is an atomic no-clobber publication on
    // the supported Unix deployment. The temporary link is cleaned on
    // return.
    match std::fs::hard_link(staged.path(), path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Error::new(
                409,
                "file_exists",
                "destination file already exists",
            ));
        }
        Err(_) => return Err(Error::internal()),
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .is_err()
    {
        // The artifact is already visible at `path`. Reporting failure while
        // leaving it there tells the operator the export did not happen when
        // it did, and the stale file then blocks the next export. Undo the
        // link so the reported state and the on-disk state agree.
        let _ = std::fs::remove_file(path);
        return Err(Error::internal());
    }
    Ok(())
}

/// Export a database to a local file.
///
/// The download is chunked: freeze a snapshot, stream it piece by
/// piece, verify the whole-file hash, then publish atomically. The
/// file appears at `out` only when it is complete and correct, and an
/// existing file is never overwritten.
pub async fn export(
    server: &str,
    key: &str,
    name: &str,
    out: &Path,
) -> std::result::Result<ExportReport, Failure> {
    // The server has already retained the snapshot named by begin. Keep
    // every subsequent validation and transfer operation inside the
    // cleanup guard so malformed metadata cannot consume a lifecycle
    // slot until the TTL.
    let begin = client::request_with_semantics(
        server,
        key,
        reqwest::Method::POST,
        &format!("/v1/databases/{name}/export/begin"),
        Some(json!({})),
        None,
        Semantics::Transfer,
    )
    .await
    .map_err(Failure::local)?;
    if api_failed(&begin) {
        return Err(Failure::refused(begin));
    }
    let snapshot = begin["snapshot"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| Failure::local(Error::internal()))?
        .to_string();
    let release_snapshot = || async {
        let _ = client::request_with_semantics(
            server,
            key,
            reqwest::Method::DELETE,
            &format!("/v1/snapshots/{snapshot}"),
            None,
            None,
            Semantics::Transfer,
        )
        .await;
    };
    let result = async {
        let size = begin["size"]
            .as_u64()
            .filter(|size| *size <= store::MAX_TRANSFER_SIZE)
            .ok_or_else(|| {
                Failure::local(Error::new(
                    413,
                    "result_limit_exceeded",
                    "export exceeds bounded response size",
                ))
            })?;
        let sha256 = begin["sha256"]
            .as_str()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| Failure::local(Error::internal()))?
            .to_string();
        let mut staged = stage_file(out).map_err(Failure::local)?;
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        while offset < size {
            let chunk = client::request(
                server,
                key,
                reqwest::Method::GET,
                &format!(
                    "/v1/snapshots/{snapshot}?offset={offset}&length={}",
                    store::MAX_CHUNK
                ),
                None,
                None,
            )
            .await
            .map_err(Failure::local)?;
            if api_failed(&chunk) {
                return Err(Failure::refused(chunk));
            }
            if chunk["snapshot"].as_str() != Some(snapshot.as_str())
                || chunk["size"].as_u64() != Some(size)
                || chunk["sha256"].as_str() != Some(sha256.as_str())
                || chunk["offset"].as_u64() != Some(offset)
            {
                return Err(Failure::local(Error::internal()));
            }
            let encoded = chunk["data_base64"]
                .as_str()
                .ok_or_else(|| Failure::local(Error::internal()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| Failure::local(Error::internal()))?;
            if bytes.is_empty()
                || bytes.len() > store::MAX_CHUNK
                || bytes.len() as u64 > size.saturating_sub(offset)
            {
                return Err(Failure::local(Error::internal()));
            }
            staged
                .write_all(&bytes)
                .map_err(|_| Failure::local(Error::internal()))?;
            hasher.update(&bytes);
            offset += bytes.len() as u64;
        }
        if offset != size || hex::encode(hasher.finalize()) != sha256 {
            return Err(Failure::local(Error::internal()));
        }
        publish_file(staged, out).map_err(Failure::local)?;
        Ok(ExportReport {
            name: name.to_string(),
            out: out.to_path_buf(),
            size,
            sha256,
        })
    }
    .await;
    release_snapshot().await;
    result
}

/// Import a local file as a new database.
///
/// The upload is chunked: stream the file piece by piece, then commit.
/// Only a single chunk is ever buffered in memory, and the commit is
/// what makes the database exist.
pub async fn import(
    server: &str,
    key: &str,
    name: &str,
    from: &Path,
) -> std::result::Result<ImportReport, Failure> {
    // Open once with O_NONBLOCK so a writer-less FIFO cannot hang the
    // caller before the descriptor is checked. The metadata check is on
    // that same descriptor, so a path replacement cannot turn this into
    // a read of a different file.
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(from)
            .map_err(|_| Failure::local(Error::invalid()))?
    };
    #[cfg(not(unix))]
    let file = std::fs::File::open(from).map_err(|_| Failure::local(Error::invalid()))?;
    let metadata = file.metadata().map_err(|_| Failure::local(Error::invalid()))?;
    // A directory, FIFO, or device is a bad path, not a size problem.
    if !metadata.is_file() {
        return Err(Failure::local(Error::invalid()));
    }
    if metadata.len() > store::MAX_TRANSFER_SIZE {
        return Err(Failure::local(Error::new(
            413,
            "result_limit_exceeded",
            "import exceeds bounded local file size",
        )));
    }
    let begin = client::request_with_semantics(
        server,
        key,
        reqwest::Method::POST,
        &format!("/v1/databases/{name}/import/begin"),
        Some(json!({})),
        None,
        Semantics::Transfer,
    )
    .await
    .map_err(Failure::local)?;
    if api_failed(&begin) {
        return Err(Failure::refused(begin));
    }
    let upload = begin["upload"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| Failure::local(Error::internal()))?
        .to_string();
    let mut reader = BufReader::new(file);
    let mut buf = vec![0u8; store::MAX_CHUNK];
    let mut offset = 0u64;
    let upload_result = async {
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|_| Failure::local(Error::invalid()))?;
            if n == 0 {
                break;
            }
            // The size check at open time is a one-shot sample of a
            // path the caller controls: the file can grow while we read
            // it, and a pseudo-file can report a small length and then
            // stream indefinitely. Enforce the ceiling on the running
            // total so the bound the error message promises is a real
            // bound, not just the server's per-append check.
            if offset + n as u64 > store::MAX_TRANSFER_SIZE {
                return Err(Failure::local(Error::new(
                    413,
                    "result_limit_exceeded",
                    "import exceeds bounded local file size",
                )));
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
            let ack = client::request_with_semantics(
                server,
                key,
                reqwest::Method::POST,
                &format!("/v1/uploads/{upload}/chunk"),
                Some(json!({"offset":offset,"data_base64":encoded})),
                None,
                Semantics::Transfer,
            )
            .await
            .map_err(Failure::local)?;
            if api_failed(&ack) {
                return Err(Failure::refused(ack));
            }
            offset += n as u64;
            if ack["received"].as_u64() != Some(offset) {
                return Err(Failure::local(Error::internal()));
            }
        }
        let done = client::request(
            server,
            key,
            reqwest::Method::POST,
            &format!("/v1/uploads/{upload}/commit"),
            Some(json!({"name":name})),
            None,
        )
        .await
        .map_err(Failure::local)?;
        if api_failed(&done) {
            return Err(Failure::refused(done));
        }
        Ok(ImportReport {
            name: name.to_string(),
            from: from.to_path_buf(),
            size: offset,
            response: done,
        })
    }
    .await;
    if upload_result.is_err() {
        // Abort is best effort: preserve the original import error and
        // never make a cleanup transport failure hide its outcome.
        let _ = client::request_with_semantics(
            server,
            key,
            reqwest::Method::DELETE,
            &format!("/v1/uploads/{upload}"),
            None,
            None,
            Semantics::Transfer,
        )
        .await;
    }
    upload_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_publication_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.sqlite");
        std::fs::write(&path, b"original").unwrap();
        let staged = stage_file(&path).unwrap();
        std::fs::write(staged.path(), b"replacement").unwrap();
        let error = publish_file(staged, &path).expect_err("an existing file was overwritten");
        assert_eq!(error.code, "file_exists");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"original",
            "the existing file was changed"
        );
    }

    #[test]
    fn a_publication_leaves_the_file_it_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.sqlite");
        let staged = stage_file(&path).unwrap();
        std::fs::write(staged.path(), b"replacement").unwrap();
        publish_file(staged, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }
}
