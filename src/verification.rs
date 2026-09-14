//! Check whether an attributed report still matches its scoped evidence.
//! The returned assessment never changes the original report or proves it ran.
use crate::{records, repository::Repository, util};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

/// Only the shared authored-report schema can acquire verification semantics.
/// Keep the original fields intact; assessments are separate read-time values.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub struct Report(Value);

impl Report {
    pub(crate) fn parse(value: &Value) -> Option<Self> {
        records::validate_verification(value).ok()?;
        if value
            .get("evidence_scope_complete")
            .is_some_and(|complete| !complete.is_boolean())
        {
            return None;
        }
        Some(Self(value.clone()))
    }

    pub(crate) fn evidence_paths(&self) -> impl Iterator<Item = &str> {
        self.0["evidence_content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|evidence| evidence["path"].as_str())
    }
}

pub(crate) fn deserialize_optional<'de, D>(deserializer: D) -> Result<Option<Report>, D::Error>
where
    D: Deserializer<'de>,
{
    // Older retained members may contain an arbitrary field under this name.
    // Their source text remains available without restoring an invalid claim.
    let value = Value::deserialize(deserializer)?;
    Ok(Report::parse(&value))
}

pub(crate) fn evaluate(repo: &Repository, report: &Report, revision: Option<&str>) -> Value {
    let report = &report.0;
    let mut applicability = "current";
    let content = report["evidence_content"].as_array();
    if content.is_none_or(Vec::is_empty) {
        applicability = "unknown";
    }
    if let Some(content) = content {
        for evidence in content {
            if let Some(path) = evidence["path"].as_str() {
                match repo.read_at(path, revision) {
                    Ok(Some(bytes)) if evidence["content_sha256"] == util::hash(&bytes) => {}
                    Ok(Some(_)) => applicability = "stale",
                    _ if applicability != "stale" => applicability = "unknown",
                    _ => (),
                }
            } else if applicability != "stale" {
                applicability = "unknown";
            }
        }
    }
    if !records::verification_scope_known(report) {
        applicability = "unknown";
    }
    let mut evaluated = report.clone();
    evaluated["claim"] = json!("reported");
    evaluated["applicability"] = json!(applicability);
    evaluated["environment_applicability"] = json!("not_checked");
    evaluated
}
