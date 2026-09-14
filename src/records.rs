use crate::config::Source;
use crate::error::{Error, Result};
use crate::redact;
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Section {
    pub heading_path: String,
    pub sha256: String,
    pub ambiguous: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Payload {
    pub kind: String,
    pub native_id: String,
    pub native_status: Option<String>,
    pub record: Value,
    pub redacted_text: String,
    pub sections: Vec<Section>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Observation {
    pub observation_id: String,
    pub version_id: String,
    pub worktree_key: String,
    pub commit: Option<String>,
    pub object_format: Option<String>,
    pub blob_oid: Option<String>,
    pub branch: String,
    pub dirty: bool,
    pub origin: String,
    pub pointer: String,
    pub source_path: String,
    pub source_sha256: String,
    #[serde(default)]
    pub source_root: Option<String>,
    pub byte_range: [u64; 2],
    pub observed_at: String,
    pub native_revision: Option<Value>,
    pub recorded_at: Option<String>,
    pub revision: String,
}

#[derive(Clone, Debug)]
pub struct Parsed {
    pub payload: Payload,
    pub selector: String,
    pub range: [u64; 2],
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub source_id: String,
    pub version_id: String,
    pub content_sha256: String,
    pub payload: Payload,
    pub observation: Observation,
}

impl Item {
    pub fn new(
        project_id: &str,
        source_id: &str,
        payload: Payload,
        mut observation: Observation,
    ) -> Result<Self> {
        let id = util::item_id(project_id, source_id, &payload.native_id);
        let content_sha256 = util::hash_json(&payload)?;
        let version_id = util::hash(format!("v1\n{id}\n{content_sha256}"));
        observation.version_id.clone_from(&version_id);
        Ok(Self {
            id,
            source_id: source_id.into(),
            version_id,
            content_sha256,
            payload,
            observation,
        })
    }
}

pub fn normalized(text: &str) -> String {
    text.nfkc()
        .filter(|c| {
            !matches!(*c, '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}')
        })
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn parse(source: &Source, path: &str, bytes: &[u8]) -> Result<Vec<Parsed>> {
    let raw = std::str::from_utf8(bytes).map_err(|_| {
        Error::new(
            "source_schema",
            json!({"source":source.id,"reason":"not UTF-8"}),
        )
    })?;
    let mut out = Vec::new();
    match source.kind.as_str() {
        "markdown" => {
            let text = redact::text(raw);
            out.push(Parsed {
                payload: Payload {
                    kind: source.kind.clone(),
                    native_id: path.into(),
                    native_status: None,
                    record: Value::Null,
                    sections: sections(&text),
                    redacted_text: text,
                },
                selector: String::new(),
                range: [0, bytes.len() as u64],
            });
        }
        "json-records" => {
            let parsed: Value = serde_json::from_str(raw).map_err(|_| {
                Error::new(
                    "source_schema",
                    json!({"source":source.id,"reason":"invalid JSON"}),
                )
            })?;
            let collection = source.collection.as_deref().expect("validated source");
            let records = parsed
                .get(collection)
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Error::new(
                        "source_schema",
                        json!({"source":source.id,"missing_collection":collection}),
                    )
                })?;
            let id_field = source.id_field.as_deref().expect("validated source");
            for record in records {
                let native = record
                    .get(id_field)
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        Error::new(
                            "source_schema",
                            json!({"source":source.id,"missing_id":id_field}),
                        )
                    })?;
                if native.is_empty() || redact::text(native) != native {
                    return Err(Error::new(
                        "source_schema",
                        "invalid or sensitive native identifier",
                    ));
                }
                let record = redact::value(record);
                let status = field_string(&record, source.policy.status_field.as_deref());
                out.push(Parsed {
                    payload: Payload {
                        kind: source.kind.clone(),
                        native_id: native.into(),
                        native_status: status,
                        redacted_text: String::from_utf8(util::canonical(&record)?)
                            .expect("JSON is UTF-8"),
                        record,
                        sections: vec![],
                    },
                    selector: format!("#{collection}/{id_field}={}", util::escape(native)),
                    range: [0, bytes.len() as u64],
                });
            }
        }
        "markdown-table" => {
            let id_column = source.id_column.as_deref().expect("validated source");
            let mut header: Option<Vec<String>> = None;
            let mut offset = 0;
            for line in raw.split_inclusive('\n') {
                let cells = table_cells(line);
                if let Some(columns) = &header {
                    if cells.len() == columns.len()
                        && !cells
                            .iter()
                            .all(|s| s.chars().all(|c| c == '-' || c == ':' || c.is_whitespace()))
                    {
                        let record = Value::Object(
                            columns
                                .iter()
                                .cloned()
                                .zip(cells.into_iter().map(Value::String))
                                .collect(),
                        );
                        let native = record.get(id_column).and_then(Value::as_str).unwrap_or("");
                        if !native.is_empty() {
                            let native = native.to_owned();
                            if redact::text(&native) != native {
                                return Err(Error::new(
                                    "source_schema",
                                    "sensitive native identifier",
                                ));
                            }
                            let record = redact::value(&record);
                            out.push(Parsed {
                                payload: Payload {
                                    kind: source.kind.clone(),
                                    native_id: native.clone(),
                                    native_status: field_string(
                                        &record,
                                        source.policy.status_field.as_deref(),
                                    ),
                                    redacted_text: String::from_utf8(util::canonical(&record)?)
                                        .expect("JSON"),
                                    record,
                                    sections: vec![],
                                },
                                selector: format!("#{id_column}={}", util::escape(&native)),
                                range: [offset, offset + line.len() as u64],
                            });
                        }
                    }
                } else if cells.iter().any(|s| s == id_column) {
                    header = Some(cells);
                }
                offset += line.len() as u64;
            }
            if header.is_none() {
                return Err(Error::new(
                    "source_schema",
                    json!({"source":source.id,"missing_column":id_column}),
                ));
            }
        }
        "memq-notes" => {
            let note: Value = serde_json::from_str(raw)
                .map_err(|_| Error::new("invalid_note", "invalid JSON"))?;
            validate_note(&note)?;
            let note = redact::value(&note);
            let native = note["id"].as_str().expect("validated").to_owned();
            out.push(Parsed {
                payload: Payload {
                    kind: source.kind.clone(),
                    native_id: native,
                    native_status: None,
                    redacted_text: note["text"].as_str().expect("validated").into(),
                    record: note,
                    sections: vec![],
                },
                selector: String::new(),
                range: [0, bytes.len() as u64],
            });
        }
        _ => return Err(Error::new("source_schema", "unknown record kind")),
    }
    let mut ids = BTreeSet::new();
    for item in &out {
        if !ids.insert(&item.payload.native_id) {
            return Err(Error::new(
                "source_schema",
                json!({"source":source.id,"reason":"duplicate native identifier"}),
            ));
        }
    }
    Ok(out)
}

pub fn validate_note(note: &Value) -> Result<()> {
    if note["format"] != 1
        || !note["id"]
            .as_str()
            .is_some_and(|s| ulid::Ulid::from_string(s).is_ok())
        || !matches!(
            note["kind"].as_str(),
            Some("note" | "progress" | "verification")
        )
        || !note["text"].is_string()
        || !note["scope"].is_object()
        || !note["scope"]["project_id"].is_string()
        || !note["provenance"]["idempotency_key"].is_string()
        || !note["recorded_at"]
            .as_str()
            .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
        || !note["evidence"].is_array()
    {
        return Err(Error::new("invalid_note", "missing or invalid note fields"));
    }
    if let Some(digest) = note["provenance"].get("idempotency_key_sha256") {
        let Some(digest) = digest.as_str().filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) else {
            return Err(Error::new("invalid_note", "invalid retry key digest"));
        };
        let stored = note["provenance"]["idempotency_key"]
            .as_str()
            .expect("validated");
        if !stored.contains("[REDACTED]") && util::hash(stored) != digest {
            return Err(Error::new(
                "invalid_note",
                "retry key digest does not match",
            ));
        }
    }
    if note["kind"] == "verification" {
        validate_verification(&note["verification"])?;
    }
    Ok(())
}

pub fn validate_verification(v: &Value) -> Result<()> {
    for key in ["command", "revision", "object_format", "reported_by"] {
        if !v[key].as_str().is_some_and(|s| !s.trim().is_empty()) {
            return Err(Error::new(
                "invalid_note",
                json!({"missing_verification_field": key}),
            ));
        }
    }
    for key in ["environment", "result"] {
        if !v[key].is_object() && !v[key].as_str().is_some_and(|s| !s.trim().is_empty()) {
            return Err(Error::new(
                "invalid_note",
                json!({"invalid_verification_field":key}),
            ));
        }
    }
    if !v["evidence"].is_array() || !v["evidence_content"].is_array() {
        return Err(Error::new(
            "invalid_note",
            "verification evidence must be arrays",
        ));
    }
    if !matches!(v["object_format"].as_str(), Some("sha1" | "sha256"))
        || !v["revision"].as_str().is_some_and(|s| {
            s.len() == if v["object_format"] == "sha1" { 40 } else { 64 }
                && s.bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return Err(Error::new("invalid_note", "invalid tested revision"));
    }
    for content in v["evidence_content"].as_array().expect("validated") {
        let path = content["path"]
            .as_str()
            .ok_or_else(|| Error::new("invalid_note", "invalid evidence path"))?;
        util::relative_path(path)?;
        if !content["content_sha256"]
            .as_str()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(Error::new("invalid_note", "invalid evidence content hash"));
        }
    }
    if v["evidence"]
        .as_array()
        .expect("validated")
        .iter()
        .any(|e| !e.as_str().is_some_and(|s| !s.is_empty()))
    {
        return Err(Error::new("invalid_note", "invalid verification evidence"));
    }
    Ok(())
}

/// Applicability concerns the declared files, never whether a command ran.
/// Unknown evidence identifiers cannot silently stand in for checked content.
pub fn verification_scope_known(v: &Value) -> bool {
    if v["evidence_scope_complete"] == false {
        return false;
    }
    let Some(evidence) = v["evidence"].as_array().filter(|a| !a.is_empty()) else {
        return false;
    };
    let Some(content) = v["evidence_content"].as_array().filter(|a| !a.is_empty()) else {
        return false;
    };
    evidence.iter().all(|e| {
        let Some(e) = e.as_str() else {
            return false;
        };
        // A Git pointer ends with :<path>#<range>. Otherwise accept a literal
        // worktree-relative path. Opaque item/session pointers need a separate
        // applicability check and remain unknown here.
        let path = if e.starts_with("git:") {
            e.splitn(4, ':')
                .nth(3)
                .map(|s| s.split('#').next().unwrap_or(s))
        } else if !e.contains(':') && util::relative_path(e).is_ok() {
            Some(e)
        } else {
            None
        };
        path.is_some_and(|path| content.iter().any(|c| c["path"] == path))
    })
}

pub fn field_string(record: &Value, field: Option<&str>) -> Option<String> {
    field
        .and_then(|f| record.get(f))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn field_values(record: &Value, field: &str) -> Vec<String> {
    match record.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => vec![],
    }
}

fn table_cells(line: &str) -> Vec<String> {
    let line = line.trim();
    if !line.starts_with('|') {
        return vec![];
    }
    let mut out = Vec::new();
    let mut cell = String::new();
    let mut escaped = false;
    let mut code = false;
    for c in line.trim_matches('|').chars() {
        if escaped {
            cell.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '`' => {
                code = !code;
                cell.push(c);
            }
            '|' if !code => {
                out.push(cell.trim().to_owned());
                cell.clear();
            }
            _ => cell.push(c),
        }
    }
    if escaped {
        cell.push('\\');
    }
    out.push(cell.trim().to_owned());
    out
}

fn sections(text: &str) -> Vec<Section> {
    let mut headings = Vec::<(usize, String)>::new();
    let mut stack = Vec::<String>::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let depth = line.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&depth) && line.as_bytes().get(depth) == Some(&b' ') {
            stack.truncate(depth - 1);
            stack.push(line[depth..].trim().to_owned());
            headings.push((offset, stack.join("/")));
        }
        offset += line.len();
    }
    let mut counts = BTreeMap::<String, usize>::new();
    for (_, h) in &headings {
        *counts.entry(h.clone()).or_default() += 1;
    }
    let mut seen = BTreeMap::<String, usize>::new();
    headings
        .iter()
        .enumerate()
        .map(|(i, (start, h))| {
            let ordinal = seen.entry(h.clone()).or_default();
            *ordinal += 1;
            let ambiguous = counts[h] > 1;
            Section {
                heading_path: if ambiguous {
                    format!("{h}[{ordinal}]")
                } else {
                    h.clone()
                },
                sha256: util::hash(
                    &text.as_bytes()[*start..headings.get(i + 1).map_or(text.len(), |(o, _)| *o)],
                ),
                ambiguous,
            }
        })
        .collect()
}
