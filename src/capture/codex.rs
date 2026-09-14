//! Codex JSONL syntax with explicit native ordinals. Never invent line IDs.
use crate::error::{Error, Result};
use crate::util;
use serde_json::Value;

pub(super) fn header(record: &Value) -> Result<Option<&Value>> {
    if record["type"] != "session_meta" || !record["ordinal"].is_u64() {
        return Err(Error::new("capture_schema", "unsupported session header"));
    }
    Ok(Some(&record["payload"]))
}

pub(super) fn entry(record: &Value, previous: &mut Option<u64>) -> Result<String> {
    let known = [
        "event_msg",
        "response_item",
        "turn_context",
        "compacted",
        "session_end",
        "world_state",
        "token_usage_record",
    ];
    let ordinal = record["ordinal"].as_u64();
    if !known.contains(&record["type"].as_str().unwrap_or(""))
        || !record["payload"].is_object()
        || ordinal.is_none()
        || previous.is_some_and(|before| ordinal.is_some_and(|value| value <= before))
    {
        return Err(Error::new(
            "capture_schema",
            "unsupported Codex entry or nonmonotonic ordinal",
        ));
    }
    *previous = ordinal;
    Ok(ordinal.expect("checked").to_string())
}

pub(super) fn pointer(relative: &str, entry: &str) -> String {
    format!("codex:{}#ordinal={entry}", util::escape(relative))
}
