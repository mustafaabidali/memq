use crate::core::{ReadRequest, Service};
use crate::error::{Error, Result};
use serde_json::{Value, json};
use std::path::Path;

/// Codex command-hook protocol, verified against the official hooks reference.
/// PostCompact cannot inject additionalContext; the next prompt refreshes it.
pub fn codex(repo: &Path, input: &Value, budget: usize, compact: bool) -> Result<Value> {
    let event = input["hook_event_name"]
        .as_str()
        .ok_or_else(|| Error::new("invalid_request", "missing hook_event_name"))?;
    let request = ReadRequest {
        budget: Some(budget),
        compact,
        ..ReadRequest::default()
    };
    match event {
        "SessionStart" | "UserPromptSubmit" => {
            let brief = Service::open(repo, false)?.read("brief", &request)?;
            Ok(json!({"hookSpecificOutput":{
                "hookEventName":event,
                "additionalContext":serde_json::to_string(&brief)?
            }}))
        }
        "PostCompact" => {
            Service::open(repo, false)?.reconcile(&request, "reconcile")?;
            Ok(json!({}))
        }
        _ => Err(Error::new("invalid_request", "unsupported memq hook event")),
    }
}
