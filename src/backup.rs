//! Offline verification of published versioned backup sets.
//!
//! Producer: `scripts/micro2-backup.sh` writes `<root>/<timestamp>/` sets
//! with `manifest.json`, per-database files plus `.sha256` sidecars, and a
//! registry copy. Reader: [`crate::store::Store::backup_sets`] skips
//! malformed sets for status purposes; [`verify_set`] re-verifies a set on
//! demand (`dg backup verify`) with the same checks the import path
//! applies, plus the full hash chain.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    error::{Error, Result},
    execution::validate_import,
    store::is_set_dir,
};

#[derive(Debug, Serialize)]
pub struct VerifiedDatabase {
    pub name: String,
    pub file: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct SetReport {
    pub set: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<String>,
    pub databases: Vec<VerifiedDatabase>,
    pub has_registry: bool,
}

fn not_found() -> Error {
    Error::new(404, "backup_set_not_found", "backup set not found")
}

fn safe_member(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\n')
        || name.contains('\r'))
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Verify the selected set (`latest` or an exact timestamp directory).
/// Returns the full report for any readable set, including failed ones;
/// only unknown selectors and unreadable roots are hard errors.
pub fn verify_set(root: &Path, selector: &str) -> Result<SetReport> {
    if !root.is_dir() {
        return Err(not_found());
    }
    let mut candidates: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    let entries = std::fs::read_dir(root).map_err(|_| not_found())?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_set_dir(&name) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|meta| meta.modified()) else {
            continue;
        };
        candidates.push((path, mtime));
    }
    let set_path = if selector == "latest" {
        candidates
            .into_iter()
            .max_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)))
            .map(|(path, _)| path)
            .ok_or_else(not_found)?
    } else {
        if !is_set_dir(selector) {
            return Err(not_found());
        }
        let path = root.join(selector);
        if !path.is_dir() {
            return Err(not_found());
        }
        path
    };
    let set = set_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut report = SetReport {
        set,
        ok: true,
        failures: Vec::new(),
        databases: Vec::new(),
        has_registry: false,
    };
    let mut fail = |reason: String| {
        report.ok = false;
        report.failures.push(reason);
    };

    let manifest_path = ["manifest.json", "MANIFEST.json"]
        .into_iter()
        .map(|name| set_path.join(name))
        .find(|path| path.is_file());
    let Some(manifest_path) = manifest_path else {
        fail("set has no manifest".to_string());
        return Ok(report);
    };
    let Some(manifest_bytes) = crate::store::read_manifest_capped(&manifest_path) else {
        fail("manifest is unreadable or exceeds the size limit".to_string());
        return Ok(report);
    };
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .ok()
        .filter(|manifest: &serde_json::Value| manifest.is_object())
        .unwrap_or(serde_json::Value::Null);
    if manifest.is_null() {
        fail("manifest is not valid JSON".to_string());
        return Ok(report);
    }
    for key in ["started_at", "completed_at", "registry_captured_at"] {
        if manifest[key].as_u64().is_none() {
            fail(format!("manifest misses {key}"));
        }
    }
    let members = manifest["databases"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if members.is_empty() {
        fail("set contains no databases".to_string());
    }
    for member in &members {
        let name = member["name"].as_str().unwrap_or("").to_string();
        let file = member["file"].as_str().unwrap_or("").to_string();
        let size = member["size"].as_u64();
        let sha256 = member["sha256"].as_str().unwrap_or("").to_string();
        if !safe_member(&name) || !safe_member(&file) || size.is_none() || !is_hex64(&sha256) {
            fail(format!("manifest entry for {name} is malformed"));
            continue;
        }
        let size = size.unwrap_or(0);
        let path = set_path.join(&file);
        let metadata = std::fs::metadata(&path).ok().filter(|meta| meta.is_file());
        let Some(metadata) = metadata else {
            fail(format!("{file} is missing"));
            continue;
        };
        if metadata.len() != size {
            fail(format!("{file} size mismatch"));
            continue;
        }
        match sha256_file(&path) {
            Ok(actual) if actual == sha256 => {}
            _ => {
                fail(format!("{file} checksum mismatch"));
                continue;
            }
        }
        let sidecar = std::fs::read_to_string(set_path.join(format!("{name}.sha256")))
            .map(|text| text.trim().to_string())
            .unwrap_or_default();
        if sidecar != sha256 {
            fail(format!("{name}.sha256 sidecar mismatch"));
            continue;
        }
        if let Err(error) = validate_import(&path) {
            fail(format!("{file}: {error}"));
            continue;
        }
        report.databases.push(VerifiedDatabase {
            name,
            file,
            size,
            sha256,
        });
    }

    let registry_file = manifest["registry"]["file"].as_str().unwrap_or("");
    let registry_size = manifest["registry"]["size"].as_u64();
    let registry_sha = manifest["registry"]["sha256"].as_str().unwrap_or("");
    if !safe_member(registry_file) || registry_size.is_none() || !is_hex64(registry_sha) {
        fail("manifest registry entry is malformed".to_string());
        return Ok(report);
    }
    let registry_path = set_path.join(registry_file);
    let registry_meta = std::fs::metadata(&registry_path)
        .ok()
        .filter(|meta| meta.is_file());
    match registry_meta {
        Some(meta) if meta.len() == registry_size.unwrap_or(0) => {}
        _ => {
            fail("registry file is missing or size-mismatched".to_string());
            return Ok(report);
        }
    }
    match sha256_file(&registry_path) {
        Ok(actual) if actual == registry_sha => {}
        _ => {
            fail("registry checksum mismatch".to_string());
            return Ok(report);
        }
    }
    // Same depth as member databases: integrity, foreign keys, and the
    // protected-table screen. A registry never legitimately carries _dg_
    // tables, so anything smuggled there fails the set like a bad member.
    if let Err(error) = validate_import(&registry_path) {
        fail(format!("registry: {error}"));
        return Ok(report);
    }
    report.has_registry = true;
    Ok(report)
}
