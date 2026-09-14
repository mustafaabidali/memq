use crate::capture;
use crate::config::Config;
use crate::error::Result;
use crate::repository::{GitState, Repository};
use crate::tombstone::Tombstone;
use crate::util;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// A source witness, independent of any SQLite connection, checked while the
/// publication transaction is still uncommitted.
pub struct Guard {
    pub repo: Repository,
    pub config: Config,
    pub tombstones: Vec<Tombstone>,
    pub variants: Vec<(String, Option<String>, String)>,
    pub branches: Vec<String>,
    pub evidence_paths: BTreeSet<(String, Option<String>)>,
    pub capture_cache: Arc<capture::CaptureCacheInput>,
    pub expected: String,
}

pub fn tombstone_files(repo: &Repository) -> Result<Vec<Value>> {
    Ok(repo
        .files_at(".memq/tombstones", None)?
        .into_iter()
        .filter(|p| p.ends_with(".json"))
        .map(|p| repo.path_fingerprint(&p, None))
        .collect())
}

#[derive(Serialize)]
pub struct Inputs<'a> {
    pub config: &'a Config,
    pub state: &'a GitState,
    pub sources: &'a [Value],
    pub branches: &'a BTreeMap<String, String>,
    pub evidence: &'a [Value],
    pub capture: &'a [Value],
    pub tombstones: &'a [Value],
    pub dirty: &'a Value,
    pub code: &'a Value,
}

impl Inputs<'_> {
    pub fn fingerprint(&self) -> Result<String> {
        util::hash_json(self)
    }
}

impl Guard {
    pub fn check(&self) -> Result<bool> {
        let config = Config::load(&self.repo.root)?;
        if util::hash_json(&config)? != util::hash_json(&self.config)? {
            return Ok(false);
        }
        let state = self.repo.state()?;
        let mut sources = Vec::new();
        for (branch, revision, origin) in &self.variants {
            for source in &config.source {
                let paths = if source.kind == "memq-notes" {
                    self.repo
                        .files_at(&source.path, revision.as_deref())?
                        .into_iter()
                        .filter(|p| p.ends_with(".json"))
                        .collect()
                } else {
                    vec![source.path.clone()]
                };
                for path in paths {
                    let dirty = revision.is_none() && state.dirty.contains_key(&path);
                    let version = match self.repo.read_at(&path, revision.as_deref()) {
                        Ok(Some(bytes)) => util::hash(bytes),
                        Ok(None) => "missing".into(),
                        Err(_) => "unreadable".into(),
                    };
                    sources.push(json!([source.id, path, branch, origin, version, dirty]));
                }
            }
        }
        let branches = self
            .branches
            .iter()
            .map(|b| self.repo.resolve_branch(b).map(|tip| (b.clone(), tip)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let evidence = self
            .evidence_paths
            .iter()
            .map(|(p, r)| json!([r, self.repo.path_fingerprint(p, r.as_deref())]))
            .collect::<Vec<_>>();
        let capture = capture::collect(
            &self.repo,
            &config.project_id,
            &config.capture,
            &self.tombstones,
            &self.capture_cache,
        )?;
        let dirty = json!(
            state
                .dirty
                .keys()
                .map(|p| self.repo.path_fingerprint(p, None))
                .collect::<Vec<_>>()
        );
        let current = Inputs {
            config: &config,
            state: &state,
            sources: &sources,
            branches: &branches,
            evidence: &evidence,
            capture: &capture.inventory,
            tombstones: &tombstone_files(&self.repo)?,
            dirty: &dirty,
            code: &crate::code_evidence::load(&self.repo, &config.project_id, &state).fingerprint,
        }
        .fingerprint()?;
        Ok(current == self.expected)
    }
}
