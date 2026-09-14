use crate::capture::CaptureCacheInput;
use crate::config::Config;
use crate::core::{ReadRequest, Service};
use crate::error::{Error, Result};
use crate::policy::{self, Member};
use crate::records::{self, Item, Observation};
use crate::repository::GitState;
use crate::store::View;
use crate::tombstone;
use crate::util;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

struct Snapshot {
    items: Vec<Item>,
    members: Vec<Member>,
    scope: Value,
    coverage: Value,
    inputs_hash: String,
    scope_hash: String,
    state: GitState,
    sources: Vec<(String, Value)>,
    cursors: Vec<(String, String, Value)>,
    capture_cache: Arc<CaptureCacheInput>,
    remote: Value,
    dirty_content: Value,
    guard: crate::publication::Guard,
}

impl Service {
    pub fn reconcile(&mut self, request: &ReadRequest, operation: &str) -> Result<View> {
        let project_id = self.config.project_id.clone();
        crate::remote::observe(
            &self.repo,
            &self.store.access,
            self.config.remote.as_ref(),
            matches!(operation, "brief" | "reconcile"),
        )?;
        let mut previous = None;
        for _ in 0..3 {
            self.config = Config::load(&self.repo.root)?;
            if self.config.project_id != project_id {
                return Err(Error::new(
                    "invalid_config",
                    "project_id changed during reconciliation",
                ));
            }
            self.tombstones = tombstone::load(&self.repo, &self.store.access)?;
            tombstone::apply(&self.store, &self.tombstones)?;
            let snapshot = {
                let capture_cache = self.store.capture_cache()?;
                self.snapshot(request, operation, &capture_cache)?
            };
            previous = self.store.previous_view(&snapshot.scope_hash)?;
            if let Some(view) = self.store.current(&snapshot.scope_hash)?
                && view.inputs_hash == snapshot.inputs_hash
                && !self.store.replacement
            {
                return Ok(view);
            }
            util::fault("after_snapshot");
            let verify_config = Config::load(&self.repo.root)?;
            if util::hash_json(&verify_config)? != util::hash_json(&self.config)? {
                continue;
            }
            let verify_tombstones = tombstone::load(&self.repo, &self.store.access)?;
            if util::hash_json(&verify_tombstones)? != util::hash_json(&self.tombstones)? {
                continue;
            }
            let same_inputs = {
                let verification = self.snapshot(request, operation, &snapshot.capture_cache)?;
                snapshot.inputs_hash == verification.inputs_hash
            };
            if !same_inputs {
                continue;
            }
            let missing_sources = snapshot.coverage["sources_missing"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            let missing = !missing_sources.is_empty();
            let repository_missing = missing_sources.iter().any(|gap| {
                self.config
                    .source
                    .iter()
                    .any(|source| gap["id"].as_str() == Some(source.id.as_str()))
            });
            let paused = snapshot.coverage["sources_paused"]
                .as_array()
                .is_some_and(|a| !a.is_empty());
            let had_items: bool = self
                .store
                .access
                .query_row(
                    "SELECT had_items FROM source_inventory WHERE worktree=?1",
                    [self.repo.root.to_string_lossy().as_ref()],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false);
            if snapshot.items.is_empty()
                && (missing || paused)
                && (had_items || self.config.source.iter().any(|s| s.kind != "memq-notes"))
            {
                if let Some(mut old) = previous.as_ref().filter(|v| !v.members.is_empty()).cloned()
                {
                    old.meta["freshness"]["status"] = json!("stale");
                    old.meta["freshness"]["reason"] = json!("recovery_limit_reached");
                    old.meta["coverage"] = snapshot.coverage;
                    return Ok(old);
                }
                if repository_missing || paused {
                    return Err(Error::new(
                        "recovery_limit_reached",
                        json!({
                            "coverage":snapshot.coverage,"reason":"supporting sources are unavailable; restore their context"
                        }),
                    ));
                }
                // An unavailable capture adapter is still a coverage gap.
                // With no surviving members, it does not make an otherwise
                // readable project source unavailable (for example after forget).
            }
            let id = util::id();
            let mut changes = json!({"baseline":null,"paths":[],"ancestry":"unknown","rationale":"not_found_in_observed_sources"});
            if let Some(before) = &previous {
                let old_head = before.meta["scope"]["head"].as_str();
                let new_head = snapshot.state.head.as_deref();
                let mut paths = BTreeSet::new();
                if let (Some(old), Some(new)) = (old_head, new_head) {
                    paths.extend(self.repo.changed_files(old, new)?);
                    changes["ancestry"] = json!(if self.repo.is_ancestor(old, new) {
                        "descendant"
                    } else {
                        "rewritten_or_divergent"
                    });
                }
                if before.meta["dirty_content"] != snapshot.dirty_content {
                    paths.extend(snapshot.state.dirty.keys().cloned());
                    if let Some(old) = before.meta["dirty_content"].as_array() {
                        paths.extend(old.iter().filter_map(|v| v[0].as_str()).map(str::to_owned));
                    }
                }
                changes["baseline"] = json!(before.id);
                changes["paths"] = json!(paths);
            }
            let status = if missing || paused || snapshot.coverage["capture_incomplete"] == true {
                "incomplete"
            } else {
                "current"
            };
            let view = View {
                id: id.clone(),
                scope_hash: snapshot.scope_hash,
                inputs_hash: snapshot.inputs_hash.clone(),
                meta: json!({
                    "scope":snapshot.scope,
                    "freshness":{"view_id":id,"published_at":util::now(),"inputs_hash":snapshot.inputs_hash,
                        "status":status,"reason":if status=="incomplete"{json!("source_coverage")}else{Value::Null},"remote":snapshot.remote},
                    "coverage":snapshot.coverage,"changes_since":changes,"dirty_content":snapshot.dirty_content
                }),
                members: snapshot.members,
            };
            let result = self.store.publish(
                &view,
                &snapshot.items,
                &snapshot.sources,
                &snapshot.cursors,
                || snapshot.guard.check(),
            );
            if result.as_ref().is_err_and(|e| e.code == "source_race") {
                continue;
            }
            result?;
            return Ok(view);
        }
        if let Some(mut view) = previous {
            view.meta["freshness"]["status"] = json!("stale");
            view.meta["freshness"]["reason"] = json!("source_race");
            return Ok(view);
        }
        Err(Error::new(
            "no_consistent_view",
            "sources changed during all reconciliation attempts",
        ))
    }

    fn snapshot(
        &self,
        request: &ReadRequest,
        operation: &str,
        capture_cache: &CaptureCacheInput,
    ) -> Result<Snapshot> {
        let state = self.repo.state()?;
        let worktree = self.repo.root.to_string_lossy().into_owned();
        let configured: BTreeSet<_> = self.config.source.iter().map(|s| s.id.clone()).collect();
        let missing_ids: Vec<_> = self
            .store
            .sources(&worktree)?
            .into_iter()
            .filter(|id| !id.starts_with("harness-") && !configured.contains(id))
            .collect();
        if !missing_ids.is_empty() && !request.allow_source_removal {
            return Err(Error::new(
                "source_identity_missing",
                json!({"source_ids":missing_ids,"remedy":"reconcile --allow-source-removal"}),
            ));
        }
        let mut branches = request.branches.clone();
        branches.sort();
        branches.dedup();
        let mut branch_tips = BTreeMap::new();
        let mut variants = Vec::new();
        if branches.is_empty() {
            variants.push((state.branch.clone(), None, "local".to_owned()));
        } else {
            for b in &branches {
                let tip = self.repo.resolve_branch(b)?;
                branch_tips.insert(b.clone(), tip.clone());
                variants.push((b.clone(), Some(tip), "local".to_owned()));
            }
        }
        let incoming = request
            .incoming
            .unwrap_or(operation == "brief" || operation == "reconcile");
        let remote = crate::remote::observe(
            &self.repo,
            &self.store.access,
            self.config.remote.as_ref(),
            false,
        )?;
        let mut incoming_config_differs = false;
        if incoming
            && remote["incoming_commits"].as_u64().is_some_and(|n| n > 0)
            && let Some(tip) = remote["tip"].as_str()
        {
            let branch = self
                .config
                .remote
                .as_ref()
                .expect("configured remote")
                .reference
                .trim_start_matches("refs/heads/")
                .to_owned();
            variants.push((branch, Some(tip.to_owned()), "incoming".into()));
            incoming_config_differs = self.repo.read_at(".memq/config.toml", Some(tip))?
                != self.repo.read_at(".memq/config.toml", None)?;
        }
        let mut items = Vec::new();
        let mut members = Vec::new();
        let mut source_set = Vec::new();
        let mut sources = Vec::new();
        let mut sources_missing = Vec::new();
        let mut paused = Vec::new();
        let mut sources_read = 0;
        let mut evidence_paths = BTreeSet::new();
        for source in &self.config.source {
            sources.push((
                source.id.clone(),
                crate::redact::value(&serde_json::to_value(source)?),
            ));
        }
        let source_paths: Vec<_> = self.config.source.iter().map(|s| s.path.as_str()).collect();
        for (branch, revision, origin) in &variants {
            let commit = revision.clone().or_else(|| state.head.clone());
            let mut blob_witness = None;
            for source in &self.config.source {
                let paths = if source.kind == "memq-notes" {
                    self.repo
                        .files_at(&source.path, revision.as_deref())?
                        .into_iter()
                        .filter(|p| p.ends_with(".json"))
                        .collect()
                } else {
                    vec![source.path.clone()]
                };
                let mut read_source = true;
                if source.kind == "memq-notes"
                    && paths.is_empty()
                    && self
                        .store
                        .note_source_seen(&worktree, &source.id, branch, origin)?
                {
                    sources_missing.push(json!({"id":source.id,"path":source.path,
                        "branch":branch,"origin":origin,"reason":"previously_observed_notes_disappeared"}));
                    read_source = false;
                }
                for path in paths {
                    let status_dirty = revision.is_none() && state.dirty.contains_key(&path);
                    let bytes = match self.repo.read_at(&path, revision.as_deref()) {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            source_set.push(json!([
                                source.id,
                                path,
                                branch,
                                origin,
                                "missing",
                                status_dirty
                            ]));
                            sources_missing.push(json!({"id":source.id,"path":path,"branch":branch,"origin":origin,"reason":"absent"}));
                            read_source = false;
                            continue;
                        }
                        Err(e) => {
                            source_set.push(json!([
                                source.id,
                                path,
                                branch,
                                origin,
                                "unreadable",
                                status_dirty
                            ]));
                            sources_missing
                                .push(json!({"id":source.id,"path":path,"reason":e.code}));
                            read_source = false;
                            continue;
                        }
                    };
                    let source_sha = util::hash(&bytes);
                    // The publication guard fingerprints raw Git status separately
                    // from the byte identity required for committed evidence.
                    source_set.push(json!([
                        source.id,
                        path,
                        branch,
                        origin,
                        source_sha,
                        status_dirty
                    ]));
                    let parsed = match records::parse(source, &path, &bytes) {
                        Ok(p) => p,
                        Err(e) => {
                            paused.push(json!({"id":source.id,"path":path,"reason":e.code}));
                            read_source = false;
                            continue;
                        }
                    };
                    let blob_oid = commit.as_deref().and_then(|rev| {
                        blob_witness
                            .get_or_insert_with(|| self.repo.blob_witness(rev, &source_paths))
                            .as_mut()
                            .ok()
                            .and_then(|witness| witness.matching_oid(&path, &bytes).ok().flatten())
                    });
                    if revision.is_some() && blob_oid.is_none() {
                        sources_missing.push(json!({
                            "id":source.id,"path":path,"branch":branch,"origin":origin,
                            "reason":"source_revision_unverified"
                        }));
                        read_source = false;
                        continue;
                    }
                    // Ignored files and assume-unchanged paths can be absent from
                    // porcelain status without containing the committed bytes.
                    let dirty = revision.is_none() && (status_dirty || blob_oid.is_none());
                    for record in parsed {
                        let note = &record.payload.record;
                        if source.kind == "memq-notes" {
                            if note["scope"]["project_id"] != self.config.project_id {
                                continue;
                            }
                            if note["scope"]["branch"]
                                .as_str()
                                .is_some_and(|b| b != branch)
                            {
                                continue;
                            }
                        }
                        let pointer = if !dirty && blob_oid.is_some() {
                            format!(
                                "git:{}@{}:{}:{}{}",
                                self.config.project_id,
                                state.object_format,
                                commit.as_deref().expect("some"),
                                util::escape(&path),
                                record.selector
                            )
                        } else {
                            format!(
                                "worktree:{}?sha256={}{}",
                                util::escape(&path),
                                source_sha,
                                record.selector
                            )
                        };
                        let recorded_at = note["recorded_at"].as_str().map(str::to_owned);
                        let observation = Observation {
                            observation_id: util::id(),
                            version_id: String::new(),
                            worktree_key: worktree.clone(),
                            commit: commit.clone(),
                            object_format: Some(state.object_format.clone()),
                            blob_oid: if dirty { None } else { blob_oid.clone() },
                            branch: branch.clone(),
                            dirty,
                            origin: origin.clone(),
                            pointer,
                            source_path: path.clone(),
                            source_sha256: source_sha.clone(),
                            source_root: None,
                            byte_range: record.range,
                            observed_at: util::now(),
                            native_revision: None,
                            recorded_at,
                            revision: commit.clone().unwrap_or_else(|| "unknown".into()),
                        };
                        let item = Item::new(
                            &self.config.project_id,
                            &source.id,
                            record.payload,
                            observation,
                        )?;
                        if capture_cache.forgotten_ids.contains(&item.id)
                            || tombstone::excludes(&self.tombstones, &item)
                        {
                            continue;
                        }
                        let member = policy::evaluate(&item, &source.policy, &source.references);
                        if let Some(report) = &member.verification {
                            for path in report.evidence_paths() {
                                evidence_paths.insert((path.to_owned(), revision.clone()));
                            }
                        }
                        for relation in &member.relations {
                            if let Some(path) = relation["to"].as_str()
                                && util::relative_path(path).is_ok()
                                && path.contains('.')
                            {
                                evidence_paths.insert((path.into(), revision.clone()));
                            }
                        }
                        members.push(member);
                        items.push(item);
                    }
                }
                if read_source {
                    sources_read += 1;
                }
            }
        }
        let mut captured = crate::capture::collect(
            &self.repo,
            &self.config.project_id,
            &self.config.capture,
            &self.tombstones,
            capture_cache,
        )?;
        captured
            .items
            .retain(|item| !capture_cache.forgotten_ids.contains(&item.id));
        let capture_cache = Arc::new(captured.cache_input());
        sources.extend(captured.sources.clone());
        sources_missing.extend(captured.missing.clone());
        for item in &captured.items {
            if item.observation.worktree_key != worktree
                || (!branches.is_empty() && !branches.contains(&item.observation.branch))
                || (branches.is_empty()
                    && item.observation.branch != state.branch
                    && item.observation.branch != "unknown")
            {
                continue;
            }
            let mut member = policy::evaluate(item, &Default::default(), &[]);
            if item.observation.branch == "unknown" {
                member.flags.push("branch_unverified".into());
            }
            members.push(member);
        }
        items.extend(captured.items);
        members.retain(|member| !capture_cache.forgotten_ids.contains(&member.id));
        policy::reconcile(&mut members);
        let code = crate::code_evidence::load(&self.repo, &self.config.project_id, &state);
        code.attach(&mut members, branches.is_empty());
        let task = request
            .task
            .clone()
            .or_else(|| {
                members
                    .iter()
                    .filter(|m| m.kind == "memq-notes" && m.task.is_some())
                    .max_by_key(|m| m.observation.recorded_at.clone())
                    .and_then(|m| m.task.clone())
            })
            .unwrap_or_else(|| "unknown".into());
        for member in &mut members {
            if member.task.as_ref().is_some_and(|t| t != &task) {
                member.required = false;
            }
        }
        members.sort_by(|a, b| {
            let priority = |m: &Member| {
                (
                    if m.required {
                        0
                    } else if m.id == task || m.task.as_ref() == Some(&task) {
                        1
                    } else if m.section == "progress" {
                        2
                    } else {
                        3
                    },
                    match m.section.as_str() {
                        "blockers" => 0,
                        "accepted_decisions" => 1,
                        "pending_decisions" => 2,
                        "resolved_upstream_not_local" => 3,
                        "progress" => 4,
                        "next_actions" => 5,
                        _ => 6,
                    },
                )
            };
            priority(a)
                .cmp(&priority(b))
                .then_with(|| b.observation.recorded_at.cmp(&a.observation.recorded_at))
                .then(a.id.cmp(&b.id))
                .then(a.observation.branch.cmp(&b.observation.branch))
                .then(a.observation.origin.cmp(&b.observation.origin))
        });
        for (rank, m) in members.iter_mut().enumerate() {
            m.rank = rank;
        }
        let scope = json!({
            "project_id":self.config.project_id,"clone_id":self.repo.clone_id,"worktree":worktree,
            "branch":state.branch,"head":state.head,"object_format":state.object_format,
            "dirty_paths":state.dirty.len(),"task":task,"branches":branches,"incoming":incoming
        });
        let scope_hash = util::hash_json(
            &json!({"worktree":worktree,"task":task,"branches":branches,"incoming":incoming}),
        )?;
        let dirty_content = json!(
            state
                .dirty
                .keys()
                .map(|p| self.repo.path_fingerprint(p, None))
                .collect::<Vec<_>>()
        );
        let evidence_content: Vec<_> = evidence_paths
            .iter()
            .map(|(p, r)| json!([r, self.repo.path_fingerprint(p, r.as_deref())]))
            .collect();
        let mut versions: BTreeSet<String> = BTreeSet::new();
        let mut stmt = self
            .store
            .db
            .prepare("SELECT version_id FROM item_versions")?;
        for version in stmt.query_map([], |r| r.get::<_, String>(0))? {
            versions.insert(version?);
        }
        versions.extend(items.iter().map(|i| i.version_id.clone()));
        let inputs_hash = util::hash_json(&json!({
            "source_set":source_set,"state":state,"scope_hash":scope_hash,"branch_tips":branch_tips,
            "config":self.config,"tombstones":self.tombstones,"evidence_paths":evidence_content,
            "dirty_content":dirty_content,"ranking_inputs":{"search":self.config.search,"fts_generation":versions,
                "frecency":self.store.frecency_hash()?,"vectors":crate::vectors::fingerprint(&self.store)?},
            "capture_generation":captured.inventory,"capture_coverage":captured.coverage,
            "forgotten_ids":capture_cache.forgotten_ids,
            "remote":remote,"memq_version":env!("CARGO_PKG_VERSION"),
            // Rebuild views that predate validated evidence extraction/scoping.
            "evidence_validation":1
            ,"code":code.fingerprint
        }))?;
        let vector_coverage = crate::vectors::coverage(
            &self.store,
            self.config.vectors.as_ref(),
            self.config.search.candidate_limit,
            &members,
        )?;
        let coverage = json!({
            "corpus":"retained_versions","sources_configured":self.config.source.len()*variants.len(),
            "sources_read":sources_read,"sources_paused":paused,"sources_missing":sources_missing,
            "capture":captured.coverage,"capture_incomplete":captured.incomplete,
            "vectors":vector_coverage["status"],"vector_details":vector_coverage,
            "code":code.coverage,
            "truncated":false,"truncated_exact":false,"query_relaxed":false,"incoming_config_differs":incoming_config_differs
        });
        let guard = crate::publication::Guard {
            repo: self.repo.clone(),
            config: self.config.clone(),
            tombstones: self.tombstones.clone(),
            variants,
            branches,
            evidence_paths,
            capture_cache: Arc::clone(&capture_cache),
            expected: crate::publication::Inputs {
                config: &self.config,
                state: &state,
                sources: &source_set,
                branches: &branch_tips,
                evidence: &evidence_content,
                capture: &captured.inventory,
                tombstones: &crate::publication::tombstone_files(&self.repo)?,
                dirty: &dirty_content,
                code: &code.fingerprint,
            }
            .fingerprint()?,
        };
        Ok(Snapshot {
            items,
            members,
            scope,
            coverage,
            inputs_hash,
            scope_hash,
            state,
            sources,
            cursors: captured.cursors,
            capture_cache,
            remote,
            dirty_content,
            guard,
        })
    }
}
