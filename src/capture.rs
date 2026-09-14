use crate::config::CaptureConfig;
use crate::error::{Error, Result};
use crate::records::{Item, Observation, Payload};
use crate::redact;
use crate::repository::Repository;
use crate::tombstone::Tombstone;
use crate::util;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

mod codex;
mod jsonl;
mod jsonl_cache;
mod omp;
mod opencode;
pub use opencode::schema as opencode_schema;

/// Prepared exclusion policy and committed capture state supplied by the core.
/// Missing or incompatible cache entries cause rescans, never missing evidence.
#[derive(Clone, Debug, Default)]
pub struct CaptureCacheInput {
    pub cursors: BTreeMap<(String, String), Value>,
    pub payloads_by_version: BTreeMap<String, Payload>,
    /// Exact identities already forgotten, independent of mutable timestamps.
    pub forgotten_ids: BTreeSet<String>,
}

impl CaptureCacheInput {
    pub fn referenced_versions(&self) -> BTreeSet<String> {
        self.cursors
            .values()
            .filter_map(jsonl_cache::Manifest::decode)
            .flat_map(|manifest| manifest.referenced_versions().collect::<Vec<_>>())
            .collect()
    }
}

/// Operational measurements only; never part of coverage or input fingerprints.
#[derive(Clone, Debug, Default, Serialize)]
pub struct CaptureWork {
    pub parsed_values: u64,
    pub redacted_records: u64,
    pub reused_records: u64,
    pub verified_prefix_bytes: u64,
    pub scope_discoveries: u64,
}

#[derive(Default)]
pub struct Captured {
    pub items: Vec<Item>,
    pub cursors: Vec<(String, String, Value)>,
    pub sources: Vec<(String, Value)>,
    pub inventory: Vec<Value>,
    pub coverage: BTreeMap<String, Value>,
    pub missing: Vec<Value>,
    pub incomplete: bool,
    pub work: CaptureWork,
    pub forgotten_ids: BTreeSet<String>,
}

impl Captured {
    /// An in-memory candidate for another validation pass. Publication still
    /// owns persistence of both cursor manifests and their immutable versions.
    pub fn cache_input(&self) -> CaptureCacheInput {
        let mut cache = CaptureCacheInput {
            forgotten_ids: self.forgotten_ids.clone(),
            ..CaptureCacheInput::default()
        };
        let mut referenced = BTreeSet::new();
        for (source, path, value) in &self.cursors {
            if let Some(manifest) = jsonl_cache::Manifest::decode(value) {
                referenced.extend(manifest.referenced_versions());
                cache
                    .cursors
                    .insert((source.clone(), path.clone()), value.clone());
            }
        }
        cache.payloads_by_version = self
            .items
            .iter()
            .filter(|item| referenced.contains(&item.version_id))
            .map(|item| (item.version_id.clone(), item.payload.clone()))
            .collect();
        cache
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    id: String,
    cwd: String,
    worktree: String,
    branch: String,
    native_revision: Option<Value>,
}

#[derive(Default)]
struct ScopeCache {
    resolved: BTreeMap<String, Option<Repository>>,
}

fn validate_metadata(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || redact::text(value) != value {
        return Err(Error::new(
            "capture_unsafe_metadata",
            json!({"field":field,"reason":"empty or sensitive native metadata"}),
        ));
    }
    Ok(())
}

fn validate_structure(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || redact::structural_has_secret(value) {
        return Err(Error::new(
            "capture_unsafe_metadata",
            json!({"field":field,"reason":"empty or sensitive native metadata"}),
        ));
    }
    Ok(())
}

fn validate_path(path: &Path, field: &str) -> Result<()> {
    // Validate components individually: a long ordinary path is not one
    // high-entropy identifier. Reject lossy paths rather than changing pointers.
    for component in path.components() {
        let value = component.as_os_str().to_str().ok_or_else(|| {
            Error::new(
                "capture_unsafe_metadata",
                json!({"field":field,"reason":"path is not UTF-8"}),
            )
        })?;
        validate_structure(value, field)?;
    }
    Ok(())
}

fn session(
    repo: &Repository,
    data: &Value,
    scopes: &mut ScopeCache,
    work: &mut CaptureWork,
) -> Result<Option<Session>> {
    let id = data["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::new("capture_schema", "missing session identity"))?;
    validate_metadata(id, "session_id")?;
    let cwd = data["cwd"]
        .as_str()
        .ok_or_else(|| Error::new("capture_schema", "missing session cwd"))?;
    validate_path(Path::new(cwd), "session_cwd")?;
    let resolved = scopes.resolved.entry(cwd.to_owned()).or_insert_with(|| {
        work.scope_discoveries += 1;
        Repository::discover(Path::new(cwd)).ok()
    });
    let resolved = resolved.as_ref().ok_or_else(|| {
        Error::new(
            "scope_unresolved",
            "session cwd cannot be resolved through Git",
        )
    })?;
    if resolved.common != repo.common {
        return Ok(None);
    }
    validate_path(&resolved.root, "session_worktree")?;
    let branch = data["git"]["branch"]
        .as_str()
        .or_else(|| data["branch"].as_str())
        .unwrap_or("unknown");
    validate_structure(branch, "session_branch")?;
    let native_revision = data.get("git").filter(|v| v.is_object()).map(|git| {
        let mut revision = redact::value(git);
        if git["branch"].is_string() {
            // Preserve only the structural value just validated above. Other
            // native Git metadata still goes through ordinary redaction.
            revision["branch"] = json!(branch);
        }
        revision
    });
    Ok(Some(Session {
        id: id.into(),
        cwd: cwd.into(),
        worktree: resolved.root.to_string_lossy().into_owned(),
        branch: branch.into(),
        native_revision,
    }))
}

pub fn collect(
    repo: &Repository,
    project: &str,
    config: &CaptureConfig,
    tombstones: &[Tombstone],
    cache: &CaptureCacheInput,
) -> Result<Captured> {
    let mut result = Captured {
        forgotten_ids: cache.forgotten_ids.clone(),
        ..Captured::default()
    };
    result.inventory.push(json!([
        "forgotten_ids",
        util::hash_json(&cache.forgotten_ids)?
    ]));
    let mut scopes = ScopeCache::default();
    for (kind, path) in [
        ("omp", &config.omp),
        ("codex", &config.codex),
        ("opencode", &config.opencode),
    ] {
        let Some(path) = path else { continue };
        let path = PathBuf::from(path);
        let source = format!("harness-{kind}");
        if let Err(error) = validate_path(&path, "source_path") {
            // Keep the source registered without persisting an unsafe root or
            // substituting a shared redaction marker for a durable path.
            result.sources.push((
                source.clone(),
                json!({"kind":kind,"root_path":null,"adapter_version":1}),
            ));
            result.inventory.push(json!([
                source,
                util::hash(path.as_os_str().as_encoded_bytes()),
                error.code
            ]));
            result
                .missing
                .push(json!({"id":source,"reason":error.code}));
            result.coverage.insert(
                source,
                json!({"status":"incomplete","accepted":0,"excluded_files":0,
                    "errors":[{"reason":error.code,"detail":error.detail}]}),
            );
            result.incomplete = true;
            continue;
        }
        let root = if kind == "opencode" {
            path.parent().unwrap_or(Path::new("."))
        } else {
            &path
        };
        result.sources.push((
            source.clone(),
            json!({"kind":kind,"root_path":root,"adapter_version":1}),
        ));
        if !path.exists() {
            result
                .missing
                .push(json!({"id":source,"path":path,"reason":"source_missing"}));
            result
                .coverage
                .insert(source, json!({"status":"source_missing","accepted":0}));
            result.incomplete = true;
            continue;
        }
        let before = result.items.len();
        let mut errors = Vec::new();
        let mut excluded = 0;
        let coalesced = if kind == "opencode" {
            match opencode::collect(repo, project, &path, tombstones, &mut scopes, &mut result) {
                Ok(issues) => errors.extend(issues),
                Err(e) => errors.push(json!({"reason":e.code,"detail":e.detail})),
            }
            0
        } else {
            let mut files = Vec::new();
            for entry in walkdir::WalkDir::new(&path).follow_links(false) {
                match entry {
                    Ok(e)
                        if e.file_type().is_file()
                            && e.path().extension().is_some_and(|s| s == "jsonl") =>
                    {
                        files.push(e.path().to_owned())
                    }
                    Ok(_) => (),
                    Err(_) => errors.push(json!({"reason":"source_unreadable"})),
                }
            }
            files.sort();
            for file in files {
                let relative_path = file.strip_prefix(root).expect("inside configured root");
                if let Err(e) = validate_path(relative_path, "source_path") {
                    result.inventory.push(json!([
                        source,
                        util::hash(relative_path.as_os_str().as_encoded_bytes()),
                        e.code
                    ]));
                    errors.push(json!({"reason":e.code,"detail":e.detail}));
                    continue;
                }
                let relative = relative_path.to_str().expect("validated path").to_owned();
                if config.exclude.iter().any(|p| relative.contains(p)) {
                    excluded += 1;
                    result.inventory.push(json!([source, relative, "excluded"]));
                    continue;
                }
                match jsonl::collect(
                    repo,
                    project,
                    kind,
                    root,
                    &file,
                    tombstones,
                    cache,
                    &mut scopes,
                    &mut result,
                ) {
                    Ok(()) => (),
                    Err(e) => {
                        errors.push(json!({"path":relative,"reason":e.code,"detail":e.detail}))
                    }
                }
            }
            jsonl::coalesce(&mut result, before, &source, &mut errors)
        };
        result.incomplete |= !errors.is_empty();
        result.coverage.insert(
            source,
            json!({
                "status":if errors.is_empty(){"ok"}else{"incomplete"},
                "accepted":result.items.len()-before,"errors":errors,"excluded_files":excluded,
                "coalesced_copies":coalesced
            }),
        );
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn make_item(
    project: &str,
    source: &str,
    session: &Session,
    entry: &str,
    text: String,
    path: &str,
    pointer: String,
    range: [u64; 2],
    raw: &Value,
    source_hash: String,
) -> Result<Item> {
    let native = format!("{}/{}", session.id, entry);
    let recorded_at = raw["timestamp"].as_str().map(redact::text);
    Item::new(
        project,
        source,
        Payload {
            kind: source.into(),
            native_id: native,
            native_status: None,
            record: Value::Null,
            redacted_text: text,
            sections: vec![],
        },
        Observation {
            observation_id: util::id(),
            version_id: String::new(),
            worktree_key: session.worktree.clone(),
            commit: None,
            object_format: None,
            blob_oid: None,
            branch: session.branch.clone(),
            dirty: false,
            origin: "local".into(),
            pointer,
            source_path: path.into(),
            source_sha256: source_hash,
            source_root: None,
            byte_range: range,
            observed_at: util::now(),
            native_revision: session.native_revision.clone(),
            recorded_at,
            revision: "unknown".into(),
        },
    )
}

/// Metadata-only probe. It never prints session IDs, cwd values, or transcript text.
pub fn probe_defaults() -> Value {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut result = BTreeMap::new();
    for (kind, root) in [
        ("omp", home.join(".omp/agent/sessions")),
        ("codex", home.join(".codex/sessions")),
    ] {
        let mut shapes = BTreeSet::new();
        let mut files = 0;
        let mut ordinal_files = 0;
        if root.exists() {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_map(std::result::Result::ok)
                .filter(|e| {
                    e.file_type().is_file() && e.path().extension().is_some_and(|s| s == "jsonl")
                })
                .take(8)
            {
                if let Ok(f) = File::open(entry.path()) {
                    files += 1;
                    for line in BufReader::new(f)
                        .lines()
                        .take(4)
                        .map_while(std::result::Result::ok)
                    {
                        if let Ok(v) = serde_json::from_str::<Value>(&line) {
                            if v["ordinal"].is_u64() {
                                ordinal_files += 1;
                            }
                            if let Some(o) = v.as_object() {
                                shapes.insert(o.keys().cloned().collect::<Vec<_>>());
                            }
                        }
                    }
                }
            }
        }
        result.insert(kind,json!({"files_sampled":files,"key_shapes":shapes,"ordinal_records_sampled":ordinal_files,"content_readout":false}));
    }
    let path = home.join(".local/share/opencode/opencode.db");
    let result_db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(Error::from)
        .and_then(|db| opencode_schema(&db));
    result.insert(
        "opencode",
        match result_db {
            Ok(v) => v,
            Err(e) => json!({"status":e.code}),
        },
    );
    json!(result)
}
