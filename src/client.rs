//! CLI transport. Never retries writes implicitly or follows redirects with keys.
use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Default credential location when neither `--key-file` nor `DG_KEY_FILE`
/// is set: `$HOME/.config/dg/key`. Returns `None` when HOME is unknown or
/// the default file is absent (callers then report missing_credential).
pub fn default_key_file() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push(".config/dg/key");
    p.is_file().then_some(p)
}

pub fn read_key(path: &Path) -> Result<String> {
    let metadata = std::fs::metadata(path).map_err(|_| Error::invalid())?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(Error::invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::new(
                400,
                "insecure_key_file",
                "credential file must not be accessible to group or others",
            ));
        }
    }
    let key = std::fs::read_to_string(path).map_err(|_| Error::invalid())?;
    let key = key.trim();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid());
    }
    Ok(key.to_string())
}
const MAX_GENERIC_RESPONSE: usize = crate::execution::MAX_RESULT + 4096;
// Export responses contain base64 for an 8 MiB SQLite snapshot plus JSON
// overhead; lifecycle responses are deliberately larger than query results.
const MAX_LIFECYCLE_RESPONSE: usize = 12 * 1024 * 1024;

pub async fn request(
    server: &str,
    key: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
    idempotency: Option<&str>,
) -> Result<Value> {
    let url = reqwest::Url::parse(server).map_err(|_| Error::invalid())?;
    let loopback = url
        .host_str()
        .map(|h| {
            h.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(Error::new(
            400,
            "invalid_server",
            "use HTTPS or a literal loopback HTTP origin",
        ));
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| Error::internal())?;
    let mut req = client
        .request(method, url.join(path).map_err(|_| Error::invalid())?)
        .bearer_auth(key);
    if let Some(body) = body {
        req = req.json(&body);
    }
    if let Some(key) = idempotency {
        req = req.header("Idempotency-Key", key);
    }
    let response=req.send().await.map_err(|_|Error::new(502,"outcome_unknown","transport failed; writes may have committed, replay only with the same idempotency key within its retention window"))?;
    let status = response.status();
    // A bounded read protects CLI memory even when pointed at a bad endpoint.
    let response_limit = if path.ends_with("/export") {
        MAX_LIFECYCLE_RESPONSE
    } else {
        MAX_GENERIC_RESPONSE
    };
    let mut response = response;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        Error::new(
            502,
            "outcome_unknown",
            "response lost; write outcome unknown",
        )
    })? {
        if bytes.len() + chunk.len() > response_limit {
            return Err(Error::new(
                502,
                "outcome_unknown",
                "response too large; write outcome unknown",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        Error::new(
            502,
            "outcome_unknown",
            "invalid response; write outcome unknown",
        )
    })?;
    // Preserve the API's stable error code and HTTP status in JSON output.
    if !status.is_success() {
        return Ok(
            json!({"error":value.get("error").cloned().unwrap_or(json!({"code":"http_error"})),"status":status.as_u16()}),
        );
    }
    Ok(value)
}
