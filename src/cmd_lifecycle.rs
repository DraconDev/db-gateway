//! Database lifecycle commands: clone, export, import, delete.

use super::{Request, api_error, api_failed, export_stage_file, human_result, publish_export_file};
use dg::ServerError::Application;
use dg::error::Error;
use reqwest::Method;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

pub async fn clone_db(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    source: String,
    destination: String,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&source).map_err(Application)?;
    dg::store::validate_name(&destination).map_err(Application)?;
    if human_result(
        server,
        json,
        key_path,
        Method::POST,
        &format!("/v1/databases/{source}/clone"),
        Some(json!({"destination":destination})),
        None,
        Box::new(|result| {
            dg::render::Outcome::done(format!(
                "cloned {} to {}",
                result["source"].as_str().unwrap_or(&source),
                result["destination"].as_str().unwrap_or(&destination),
            ))
        },
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::POST,
        format!("/v1/databases/{source}/clone"),
        Some(json!({"destination":destination})),
        None,
    )))
}

pub async fn export(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    out: PathBuf,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    // Chunked download: freeze a snapshot, stream it piece by piece,
    // verify the whole-file hash, then publish atomically.
    let begin = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        &format!("/v1/databases/{name}/export/begin"),
        Some(json!({})),
        None,
        dg::client::Semantics::Transfer,
    )
    .await
    .map_err(Application)?;
    if api_failed(&begin) {
        println!("{begin}");
        return Err(Application(api_error()));
    }
    let snapshot = begin["snapshot"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(Application(Error::internal()))?
        .to_string();
    let release_snapshot = || async {
        let _ = dg::client::request_with_semantics(
            server,
            &key,
            Method::DELETE,
            &format!("/v1/snapshots/{snapshot}"),
            None,
            None,
            dg::client::Semantics::Transfer,
        )
        .await;
    };
    let result = async {
        // The server has already retained the snapshot named by
        // begin. Keep every subsequent validation and transfer
        // operation inside the cleanup guard so malformed metadata
        // cannot consume a lifecycle slot until the TTL.
        let size = begin["size"]
            .as_u64()
            .filter(|size| *size <= dg::store::MAX_TRANSFER_SIZE)
            .ok_or_else(|| {
                Application(Error::new(
                    413,
                    "result_limit_exceeded",
                    "export exceeds bounded response size",
                ))
            })?;
        let sha256 = begin["sha256"]
            .as_str()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or(Application(Error::internal()))?
            .to_string();
        let mut staged = export_stage_file(&out).map_err(Application)?;
        let mut hasher = Sha256::new();
        let mut offset = 0u64;
        while offset < size {
            let chunk = dg::client::request(
                server,
                &key,
                Method::GET,
                &format!(
                    "/v1/snapshots/{snapshot}?offset={offset}&length={}",
                    dg::store::MAX_CHUNK
                ),
                None,
                None,
            )
            .await
            .map_err(Application)?;
            if api_failed(&chunk) {
                println!("{chunk}");
                return Err(Application(api_error()));
            }
            if chunk["snapshot"].as_str() != Some(snapshot.as_str())
                || chunk["size"].as_u64() != Some(size)
                || chunk["sha256"].as_str() != Some(sha256.as_str())
                || chunk["offset"].as_u64() != Some(offset)
            {
                return Err(Application(Error::internal()));
            }
            let encoded = chunk["data_base64"]
                .as_str()
                .ok_or(Application(Error::internal()))?;
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                .map_err(|_| Application(Error::internal()))?;
            if bytes.is_empty()
                || bytes.len() > dg::store::MAX_CHUNK
                || bytes.len() as u64 > size.saturating_sub(offset)
            {
                return Err(Application(Error::internal()));
            }
            staged
                .write_all(&bytes)
                .map_err(|_| Application(Error::internal()))?;
            hasher.update(&bytes);
            offset += bytes.len() as u64;
        }
        if offset != size || hex::encode(hasher.finalize()) != sha256 {
            return Err(Application(Error::internal()));
        }
        publish_export_file(staged, &out).map_err(Application)?;
        Ok((size, sha256))
    }
    .await;
    release_snapshot().await;
    let (size, sha256) = result?;
    let report = json!({"name":name,"out":out.display().to_string(),"size":size,"sha256":sha256});
    if json || !std::io::stdout().is_terminal() {
        println!("{report}");
        return Ok(());
    }
    // A person wants to know it worked and that the file is intact:
    // the size it is, and the hash to check it against.
    let mut stdout = std::io::stdout();
    dg::render::Outcome::with_facts(
        format!("exported {name}"),
        vec![
            (
                "file".to_string(),
                out.display().to_string(),
            ),
            (
                "size".to_string(),
                dg::render::human_bytes(size),
            ),
            ("sha256".to_string(), sha256),
        ],
    )
    .write(&mut stdout, &dg::render::Theme::for_stream(true))
    .map_err(Application)?;
    Ok(())
}

pub async fn import(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    from: PathBuf,
) -> Result<(), dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    let key = dg::client::read_key(key_path.as_deref().ok_or(Application(Error::new(
        400,
        "missing_credential",
        "--key-file required",
    )))?)
    .map_err(Application)?;
    // Chunked upload: stream the file piece by piece, then commit.
    // Only a single chunk is ever buffered in memory.
    // Open with O_NONBLOCK (like the batch path) so a writer-less
    // FIFO cannot hang the CLI before the regular-file check below.
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&from)
        .map_err(|_| Application(Error::invalid()))?;
    #[cfg(not(unix))]
    let file = std::fs::File::open(&from).map_err(|_| Application(Error::invalid()))?;
    let metadata = file.metadata().map_err(|_| Application(Error::invalid()))?;
    // A directory, FIFO, or device is a bad path, not a size problem.
    // Folding the type check into the 413 below answered "too large"
    // for input the sibling `dg batch` path calls invalid.
    if !metadata.is_file() {
        return Err(Application(Error::invalid()));
    }
    if metadata.len() > dg::store::MAX_TRANSFER_SIZE {
        return Err(Application(Error::new(
            413,
            "result_limit_exceeded",
            "import exceeds bounded local file size",
        )));
    }
    let begin = dg::client::request_with_semantics(
        server,
        &key,
        Method::POST,
        &format!("/v1/databases/{name}/import/begin"),
        Some(json!({})),
        None,
        dg::client::Semantics::Transfer,
    )
    .await
    .map_err(Application)?;
    if api_failed(&begin) {
        println!("{begin}");
        return Err(Application(api_error()));
    }
    let upload = begin["upload"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or(Application(Error::internal()))?
        .to_string();
    let mut reader = BufReader::new(file);
    let mut buf = vec![0u8; dg::store::MAX_CHUNK];
    let mut offset = 0u64;
    let upload_result = async {
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|_| Application(Error::invalid()))?;
            if n == 0 {
                break;
            }
            // The size check at open time is a one-shot sample of a
            // path the caller controls: the file can grow while we
            // read it, and a pseudo-file can report a small length and
            // then stream indefinitely. Enforce the ceiling on the
            // running total so the bound the error message promises
            // is a real bound, not just the server's per-append check.
            if offset + n as u64 > dg::store::MAX_TRANSFER_SIZE {
                return Err(Application(Error::new(
                    413,
                    "result_limit_exceeded",
                    "import exceeds bounded local file size",
                )));
            }
            let encoded =
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &buf[..n]);
            let ack = dg::client::request_with_semantics(
                server,
                &key,
                Method::POST,
                &format!("/v1/uploads/{upload}/chunk"),
                Some(json!({"offset":offset,"data_base64":encoded})),
                None,
                dg::client::Semantics::Transfer,
            )
            .await
            .map_err(Application)?;
            if api_failed(&ack) {
                println!("{ack}");
                return Err(Application(api_error()));
            }
            offset += n as u64;
            if ack["received"].as_u64() != Some(offset) {
                return Err(Application(Error::internal()));
            }
        }
        let done = dg::client::request(
            server,
            &key,
            Method::POST,
            &format!("/v1/uploads/{upload}/commit"),
            Some(json!({"name":name})),
            None,
        )
        .await
        .map_err(Application)?;
        if json || !std::io::stdout().is_terminal() {
            println!("{done}");
        } else {
            let mut stdout = std::io::stdout();
            dg::render::Outcome::with_facts(
                format!("imported {name}"),
                vec![
                    ("file".to_string(), from.display().to_string()),
                    (
                        "size".to_string(),
                        dg::render::human_bytes(
                            std::fs::metadata(&from).map(|meta| meta.len()).unwrap_or(0),
                        ),
                    ),
                ],
            )
            .write(&mut stdout, &dg::render::Theme::for_stream(true))
            .map_err(Application)?;
        }
        if api_failed(&done) {
            return Err(Application(api_error()));
        }
        Ok::<(), dg::ServerError>(())
    }
    .await;
    if upload_result.is_err() {
        // Abort is best effort: preserve the original import error and
        // never make a cleanup transport failure hide its outcome.
        let _ = dg::client::request_with_semantics(
            server,
            &key,
            Method::DELETE,
            &format!("/v1/uploads/{upload}"),
            None,
            None,
            dg::client::Semantics::Transfer,
        )
        .await;
    }
    upload_result?;
    Ok(())
}

pub async fn delete(
    server: &str,
    json: bool,
    key_path: &Option<PathBuf>,
    name: String,
    confirm: Option<String>,
) -> Result<Option<Request>, dg::ServerError> {
    dg::store::validate_name(&name).map_err(Application)?;
    if confirm.as_deref() != Some(name.as_str()) {
        return Err(Application(Error::new(
            400,
            "confirmation_required",
            "destructive operation: repeat the exact database name with --confirm NAME",
        )));
    }
    if human_result(
        server,
        json,
        key_path,
        Method::DELETE,
        &format!("/v1/databases/{name}"),
        None,
        None,
        Box::new(|result| {
            // A tombstone is not a deletion: the name is taken until
            // the retention window purges it, and the operator has to
            // know when it comes back.
            let purge_after = result["purge_after_seconds"]
                .as_u64()
                .map(dg::render::human_duration)
                .unwrap_or_else(|| "unknown".to_string());
            dg::render::Outcome::with_facts(
                format!("tombstoned {name}"),
                vec![("purges in".to_string(), purge_after)],
            )
        }),
    )
    .await?
    .is_some()
    {
        return Ok(None);
    }
    Ok(Some((
        Method::DELETE,
        format!("/v1/databases/{name}"),
        None,
        None,
    )))
}
