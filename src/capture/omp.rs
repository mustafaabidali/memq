//! OMP v3 JSONL syntax. Scope checks and evidence persistence stay shared.
use crate::error::{Error, Result};
use crate::util;
use serde_json::Value;

pub(super) fn header(record: &Value) -> Result<Option<&Value>> {
    if record["type"] == "title" {
        return Ok(None);
    }
    if record["type"] != "session" || record["version"] != 3 {
        return Err(Error::new("capture_schema", "unsupported session header"));
    }
    Ok(Some(record))
}

pub(super) fn entry(record: &Value) -> Result<String> {
    let known = [
        "message",
        "model_usage",
        "thinking_level_change",
        "model_change",
        "service_tier_change",
        "compaction",
        "branch_summary",
        "reset_boundary",
        "custom",
        "label",
        "title_change",
        "ttsr_injection",
        "credential_pin",
        "session_init",
        "mode_change",
        "custom_message",
    ];
    if !known.contains(&record["type"].as_str().unwrap_or("")) || !record["id"].is_string() {
        return Err(Error::new("capture_schema", "unsupported OMP entry"));
    }
    Ok(record["id"].as_str().expect("checked").to_owned())
}

pub(super) fn pointer(relative: &str, entry: &str) -> String {
    format!("omp:{}#{}", util::escape(relative), util::escape(entry))
}
