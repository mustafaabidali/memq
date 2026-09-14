use crate::error::{Error, Result};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub format: u32,
    pub project_id: String,
    #[serde(default)]
    pub brief: BriefConfig,
    #[serde(default)]
    pub search: SearchConfig,
    pub remote: Option<RemoteConfig>,
    #[serde(default)]
    pub source: Vec<Source>,
    #[serde(default)]
    pub capture: CaptureConfig,
    pub vectors: Option<VectorConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BriefConfig {
    pub budget: usize,
    pub budget_kind: String,
    pub tokenizer: String,
}

impl Default for BriefConfig {
    fn default() -> Self {
        Self {
            budget: 4000,
            budget_kind: "tokens".into(),
            tokenizer: "o200k_base".into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    pub fusion_k: usize,
    pub candidate_limit: usize,
    pub result_limit: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            fusion_k: 60,
            candidate_limit: 50,
            result_limit: 20,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub kind: String,
    pub path: String,
    pub collection: Option<String>,
    pub id_field: Option<String>,
    pub id_column: Option<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub policy: Policy,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub status_field: Option<String>,
    pub accepted_values: Vec<String>,
    pub proposed_values: Vec<String>,
    pub withdrawn_values: Vec<String>,
    pub superseded_values: Vec<String>,
    pub blocked_values: Vec<String>,
    pub active_values: Vec<String>,
    pub supersedes_field: Option<String>,
    pub superseded_by_field: Option<String>,
    pub reason_field: Option<String>,
    pub decider_field: Option<String>,
    pub approvers: Option<Vec<String>>,
    pub require_attribution: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    pub omp: Option<String>,
    pub codex: Option<String>,
    pub opencode: Option<String>,
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VectorConfig {
    /// Optional local embedding process. JSON on stdin, JSON on stdout.
    pub command: Vec<String>,
    pub model: String,
    pub dims: usize,
    pub preprocessing_version: String,
    #[serde(default = "vector_timeout")]
    pub timeout_seconds: u64,
}

fn vector_timeout() -> u64 {
    60
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteConfig {
    pub name: String,
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default = "remote_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "remote_interval")]
    pub min_interval_seconds: u64,
}

fn remote_timeout() -> u64 {
    20
}
fn remote_interval() -> u64 {
    300
}

impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let path = fs::canonicalize(root.join(".memq/config.toml"))
            .map_err(|_| Error::new("invalid_config", "run memq init in this repository first"))?;
        let bytes = crate::repository::read_regular_file(&path).map_err(|error| {
            Error::new(
                "invalid_config",
                if error.code == "source_not_regular" {
                    "configuration must be a regular file"
                } else {
                    "cannot read .memq/config.toml"
                },
            )
        })?;
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::new("invalid_config", "configuration must be UTF-8"))?;
        let mut config = Self::parse(&text)?;
        for (name, target) in [
            ("MEMQ_OMP_STORE", &mut config.capture.omp),
            ("MEMQ_CODEX_STORE", &mut config.capture.codex),
            ("MEMQ_OPENCODE_STORE", &mut config.capture.opencode),
        ] {
            if let Ok(path) = std::env::var(name) {
                *target = (!path.is_empty()).then_some(path);
            }
        }
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let c: Self = toml::from_str(text).map_err(|e: toml::de::Error| {
            // The TOML source may contain secrets. Never echo its contextual excerpt.
            Error::new("invalid_config", json!({"message": e.message()}))
        })?;
        if c.format != 1 || ulid::Ulid::from_string(&c.project_id).is_err() {
            return Err(Error::new(
                "invalid_config",
                "format must be 1 and project_id must be a ULID",
            ));
        }
        if !matches!(c.brief.budget_kind.as_str(), "tokens" | "bytes")
            || !matches!(c.brief.tokenizer.as_str(), "o200k_base" | "cl100k_base")
            || c.search.candidate_limit == 0
            || c.search.result_limit == 0
            || c.search.fusion_k == 0
        {
            return Err(Error::new(
                "invalid_config",
                "invalid brief or search setting",
            ));
        }
        let mut ids = BTreeSet::new();
        for s in &c.source {
            if s.id.is_empty()
                || s.id.len() > 32
                || !s
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
                || !s.id.as_bytes()[0].is_ascii_alphanumeric()
                || ["harness-omp", "harness-codex", "harness-opencode"].contains(&s.id.as_str())
                || !ids.insert(&s.id)
            {
                return Err(Error::new(
                    "invalid_config",
                    json!({"key": "source.id", "id": s.id}),
                ));
            }
            util::relative_path(&s.path)?;
            match s.kind.as_str() {
                "memq-notes" | "markdown" => (),
                "json-records" if s.collection.is_some() && s.id_field.is_some() => (),
                "markdown-table" if s.id_column.is_some() => (),
                _ => {
                    return Err(Error::new(
                        "invalid_config",
                        json!({"key": "source.kind", "id": s.id, "kind": s.kind}),
                    ));
                }
            }
        }
        if let Some(v) = &c.vectors
            && (v.command.is_empty()
                || v.command[0].is_empty()
                || v.model.is_empty()
                || v.dims == 0
                || v.dims > 65_536
                || v.preprocessing_version.is_empty()
                || v.timeout_seconds == 0)
        {
            return Err(Error::new(
                "invalid_config",
                "invalid vectors configuration",
            ));
        }
        if let Some(r) = &c.remote
            && (r.timeout_seconds == 0 || r.timeout_seconds > 300)
        {
            return Err(Error::new(
                "invalid_config",
                "remote.timeout_seconds must be between 1 and 300",
            ));
        }
        Ok(c)
    }

    pub fn init(root: &Path) -> Result<Self> {
        let dir = root.join(".memq");
        fs::create_dir_all(&dir)?;
        let text = format!(
            "format = 1\nproject_id = \"{}\"\n\n# These defaults may change. See docs/usage.md in the memq source.\n[brief]\nbudget = 4000\nbudget_kind = \"tokens\"\ntokenizer = \"o200k_base\"\n\n[[source]]\nid = \"notes\"\nname = \"Progress notes\"\nkind = \"memq-notes\"\npath = \".memq/notes\"\n",
            util::id()
        );
        util::write_private(&dir.join("config.toml"), text.as_bytes()).map_err(|error| {
            // Preserve create_new's atomic no-replace guarantee, including
            // concurrent init attempts and existing symlinks or special files.
            if error.code == "io_error" && error.detail["kind"] == "AlreadyExists" {
                Error::new(
                    "invalid_config",
                    "memq is already initialized; keep .memq/config.toml and run memq brief",
                )
            } else {
                error
            }
        })?;
        util::sync_dir(&dir)?;
        Self::parse(&text)
    }
}
