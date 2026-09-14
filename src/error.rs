use serde_json::{Value, json};
use std::fmt;

#[derive(Debug)]
pub struct Error {
    pub code: String,
    pub detail: Value,
    pub exit: i32,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(code: &str, detail: impl Into<Value>) -> Self {
        let exit = match code {
            "idempotency_conflict" | "duplicate_authored_notes" => 3,
            "in_progress" => 4,
            "not_a_git_repository"
            | "invalid_config"
            | "invalid_request"
            | "budget_below_minimum"
            | "stale_continuation"
            | "source_identity_missing"
            | "query_error"
            | "invalid_note"
            | "invalid_tombstone" => 2,
            _ => 1,
        };
        Self {
            code: code.into(),
            // Parsers may quote an invalid value in their diagnostics. Sanitize
            // at construction so both returned and retained errors are safe.
            detail: crate::redact::value(&detail.into()),
            exit,
        }
    }

    pub fn envelope(&self, operation: &str) -> Value {
        json!({
            "memq": {"version": env!("CARGO_PKG_VERSION"), "envelope": 1, "operation": operation},
            "error": {"code": self.code, "detail": self.detail}
        })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new("io_error", json!({"kind": format!("{:?}", e.kind())}))
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        // Database diagnostics do not contain source text or SQL parameters.
        Self::new("storage_error", json!({"message": e.to_string()}))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(
            "invalid_request",
            json!({"format": "json", "line": e.line(), "column": e.column()}),
        )
    }
}
