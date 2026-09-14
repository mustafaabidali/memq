use crate::error::{Error, Result};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path};
use ulid::Ulid;

pub fn canonical<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    serde_jcs::to_vec(v).map_err(|_| Error::new("invalid_request", "cannot canonicalize JSON"))
}

pub fn hash(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

pub fn hash_json(v: &impl Serialize) -> Result<String> {
    Ok(hash(canonical(v)?))
}

pub fn id() -> String {
    Ulid::new().to_string()
}

pub fn now() -> String {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("MEMQ_NOW")
        && chrono::DateTime::parse_from_rfc3339(&value).is_ok()
    {
        return value;
    }
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn escape(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn unescape(encoded: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let input = encoded.as_bytes();
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hex = encoded
                .get(i + 1..i + 3)
                .ok_or_else(|| Error::new("invalid_request", "invalid percent encoding"))?;
            bytes.push(
                u8::from_str_radix(hex, 16)
                    .map_err(|_| Error::new("invalid_request", "invalid percent encoding"))?,
            );
            i += 3;
        } else {
            bytes.push(input[i]);
            i += 1;
        }
    }
    let raw = String::from_utf8(bytes)
        .map_err(|_| Error::new("invalid_request", "identity is not UTF-8"))?;
    if escape(&raw) != encoded {
        return Err(Error::new("invalid_request", "noncanonical identity"));
    }
    Ok(raw)
}

pub fn item_id(project: &str, source: &str, native: &str) -> String {
    format!("mq:{project}:{source}:{}", escape(native))
}

pub fn relative_path(path: &str) -> Result<()> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::new(
            "invalid_config",
            json!({"path": path, "reason": "expected repository-relative path"}),
        ));
    }
    Ok(())
}

pub fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Publish a new authored file. A hard link is an atomic no-replace operation.
pub fn publish_new(temp: &Path, final_path: &Path) -> Result<()> {
    fs::hard_link(temp, final_path)?;
    sync_dir(final_path.parent().expect("file has parent"))?;
    fs::remove_file(temp)?;
    sync_dir(final_path.parent().expect("file has parent"))
}

pub fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension(format!("{}.tmp", id()));
    write_private(&temp, bytes)?;
    fs::rename(&temp, path)?;
    sync_dir(path.parent().expect("file has parent"))
}

/// Fault injection is absent from release binaries.
pub fn fault(point: &str) {
    #[cfg(debug_assertions)]
    {
        if std::env::var("MEMQ_PAUSE_AT").as_deref() == Ok(point)
            && let Ok(path) = std::env::var("MEMQ_PAUSE_FILE")
        {
            let _ = fs::write(&path, point);
            while Path::new(&path).exists() {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        if std::env::var("MEMQ_FAULT").as_deref() == Ok(point) {
            std::process::exit(86);
        }
    }
    let _ = point;
}
