//! Optional reports from an external code graph. This module neither indexes
//! code nor invokes a graph provider. File tools remain responsible for text.
use crate::policy::Member;
use crate::repository::{GitState, Repository};
use crate::{redact, util};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    format: u32,
    tool: String,
    tool_version: String,
    report_id: String,
    project_id: String,
    worktree: String,
    checked_revision: String,
    object_format: String,
    complete: bool,
    files: Vec<FileEvidence>,
    relations: Vec<Relation>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEvidence {
    path: String,
    content_sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Relation {
    from: String,
    to: String,
    relationship: String,
}

pub struct Evidence {
    pub coverage: Value,
    pub fingerprint: Value,
    relations: Vec<Value>,
}

pub fn load(repo: &Repository, project: &str, state: &GitState) -> Evidence {
    let unavailable = |reason: &str, fingerprint: Value| Evidence {
        coverage: json!({"tool":"codebase-memory-mcp","status":"unavailable",
            "checked_revision":null,"reason":reason}),
        fingerprint,
        relations: vec![],
    };
    let Some(path) = std::env::var_os("MEMQ_CODE_REPORT").filter(|p| !p.is_empty()) else {
        return Evidence {
            coverage: json!({"tool":null,"status":"unavailable","checked_revision":null}),
            fingerprint: Value::Null,
            relations: vec![],
        };
    };
    let Ok(bytes) = fs::read(path) else {
        return unavailable("report_unreadable", json!("unreadable"));
    };
    let hash = json!(util::hash(&bytes));
    let Ok(report) = serde_json::from_slice::<Report>(&bytes) else {
        return unavailable("report_schema_unsupported", hash);
    };
    if report.format != 1
        || report.tool != "codebase-memory-mcp"
        || report.tool_version.is_empty()
        || report.report_id.is_empty()
        || report.project_id != project
        || report.worktree != util::hash(repo.root.to_string_lossy().as_bytes())
        || report.files.iter().any(|f| {
            util::relative_path(&f.path).is_err()
                || f.content_sha256.len() != 64
                || !f.content_sha256.bytes().all(|b| b.is_ascii_hexdigit())
        })
        || report
            .relations
            .iter()
            .any(|r| r.from.is_empty() || r.to.is_empty() || r.relationship.is_empty())
    {
        return unavailable("report_scope_or_schema_mismatch", hash);
    }
    let mut fingerprints = vec![];
    let mut current = state.head.as_ref() == Some(&report.checked_revision)
        && state.object_format == report.object_format;
    for f in &report.files {
        let actual = repo.path_fingerprint(&f.path, None);
        fingerprints.push(actual);
        current &= repo
            .read_at(&f.path, None)
            .is_ok_and(|b| b.is_some_and(|b| util::hash(b) == f.content_sha256));
    }
    let status = if !current {
        "outdated"
    } else if !report.complete || report.files.is_empty() {
        "partial"
    } else {
        "current"
    };
    let checked_scope = json!({
        "kind":"live_checkout","worktree":report.worktree,
        "branch":state.branch,"origin":"local"
    });
    let coverage = redact::value(&json!({
        "tool":report.tool,"tool_version":report.tool_version,
        "report_id":report.report_id,"status":status,
        "checked_revision":report.checked_revision,"object_format":report.object_format,
        "covered_files":report.files.len(),"complete":report.complete,
        "scope":"reported_files_only","checked_scope":checked_scope
    }));
    let relations = report
        .relations
        .iter()
        .map(|r| {
            redact::value(&json!({
                "kind":"code","from":r.from,"to":r.to,"relationship":r.relationship,
                "tool":report.tool,"tool_version":report.tool_version,
                "checked_revision":report.checked_revision,"coverage_report_id":report.report_id,
                "status":status,"to_version_id":null,"checked_scope":checked_scope
            }))
        })
        .collect();
    Evidence {
        coverage,
        fingerprint: json!([hash, fingerprints, checked_scope]),
        relations,
    }
}

impl Evidence {
    pub fn attach(&self, members: &mut [Member], live_checkout: bool) {
        for member in members {
            // The report's file hashes were checked against working bytes.
            // A named branch at the same HEAD may still contain different code.
            let in_checked_scope = live_checkout
                && member.observation.origin == "local"
                && self.coverage["checked_scope"]["branch"] == member.observation.branch
                && self.coverage["checked_scope"]["worktree"]
                    == util::hash(member.observation.worktree_key.as_bytes());
            for relation in &self.relations {
                let from = relation["from"].as_str().unwrap_or("");
                if from == member.id
                    || from == member.observation.source_path
                    || member
                        .relations
                        .iter()
                        .any(|r| r["kind"] == "authored" && r["to"] == from)
                {
                    let mut relation = relation.clone();
                    relation["from_version_id"] = json!(member.version_id);
                    if !in_checked_scope {
                        relation["report_status"] = relation["status"].clone();
                        relation["status"] = json!("outside_checked_scope");
                    }
                    member.relations.push(relation);
                }
            }
        }
    }
}
