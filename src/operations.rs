use crate::budget::{Budget, Continuation, utf8_end};
use crate::core::{ReadRequest, Service};
use crate::error::{Error, Result};
use crate::policy::Member;
use crate::presentation;
use crate::presentation::{ReplyBudget, compact as compact_item, fragment as render, omission};
use crate::retrieval::{self, Candidate};
use crate::store::View;
use crate::tombstone;
use crate::util;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Eq, PartialEq, Hash)]
enum AvailabilitySource {
    Repository { root: PathBuf, path: String },
    Harness { root: Option<String>, path: String },
}

type AvailabilityCache = HashMap<AvailabilitySource, bool>;

struct Evidence {
    item: Value,
    text: String,
    id: String,
    required: bool,
}

impl Service {
    pub fn read(&mut self, operation: &str, request: &ReadRequest) -> Result<Value> {
        if !matches!(operation, "brief" | "search" | "show") {
            return Err(Error::new("invalid_request", "unknown read operation"));
        }
        let continuation = request
            .continuation
            .as_deref()
            .map(Continuation::decode)
            .transpose()?;
        let mut view = if let Some(c) = &continuation {
            self.store.view(&c.view_id)?
        } else if let Some(id) = &request.view_id {
            self.store.view(id)?
        } else {
            self.reconcile(request, operation)?
        };
        self.validate_view_scope(&view, request)?;
        let forgotten_ids = self.store.forgotten_ids()?;
        if operation != "show" {
            // Filter before lexical/vector candidate limits, including when
            // reading a retained view. Explicit show requests still get a
            // forgotten placeholder instead of silently losing an argument.
            view.members.retain(|member| {
                !tombstone::excludes_record(
                    &self.tombstones,
                    &forgotten_ids,
                    &member.id,
                    &member.source_id,
                    member.observation.recorded_at.as_deref(),
                )
            });
        }
        let mut branches = request.branches.clone();
        branches.sort();
        branches.dedup();
        let mut selection = json!({
            "operation":operation,"query":request.query,"ids":request.ids,
            "scope":{"worktree":self.repo.root,"project_id":self.config.project_id,"task":request.task,
                "branches":branches,"incoming":request.incoming.unwrap_or(operation=="brief")},
            "ranking_inputs":view.inputs_hash,"current_tombstones":util::hash_json(&self.tombstones)?
        });
        if !forgotten_ids.is_empty() {
            selection["forgotten_ids"] = json!(forgotten_ids);
        }
        let selection_hash = util::hash_json(&selection)?;
        // Presentation changes cursor identity, not retrieval or its saved order.
        // Keep the existing full-mode hash so its retained cursors still work.
        let request_hash = if request.compact {
            util::hash_json(&json!({"selection":selection_hash,"representation":"compact-v2"}))?
        } else {
            selection_hash.clone()
        };
        if let Some(c) = &continuation
            && c.request_hash != request_hash
        {
            return Err(Error::new(
                "stale_continuation",
                "continuation belongs to another request or scope",
            ));
        }
        let budget = ReplyBudget::new(Budget {
            kind: request
                .budget_kind
                .clone()
                .unwrap_or_else(|| self.config.brief.budget_kind.clone()),
            encoding: request
                .tokenizer
                .clone()
                .unwrap_or_else(|| self.config.brief.tokenizer.clone()),
            limit: request.budget.unwrap_or(self.config.brief.budget),
        })?;
        let mut envelope = self.envelope(operation, &view, request.compact)?;
        let token = |stream: &str, offset: usize| -> Result<Value> {
            Ok(json!(
                Continuation {
                    format: 1,
                    view_id: view.id.clone(),
                    request_hash: request_hash.clone(),
                    stream: stream.into(),
                    offset
                }
                .encode()?
            ))
        };
        if operation == "show" {
            if continuation
                .as_ref()
                .is_some_and(|c| c.stream != "evidence")
            {
                return Err(Error::new(
                    "stale_continuation",
                    "wrong continuation stream",
                ));
            }
            if request.ids.is_empty() {
                return Err(Error::new(
                    "invalid_request",
                    "show requires item identifiers",
                ));
            }
            return self.show_page(
                &view,
                request,
                &budget,
                envelope,
                continuation.as_ref().map_or(0, |c| c.offset),
                &token,
            );
        }
        let candidates: Vec<Candidate> = if operation == "brief" {
            let mut members = view.members.clone();
            members.sort_by_key(|m| m.rank);
            members
                .into_iter()
                .map(|member| Candidate {
                    member,
                    paths: vec![],
                    semantic: None,
                })
                .collect()
        } else {
            let key = util::hash(format!("{}:{selection_hash}", view.id));
            let saved: Option<String> = self
                .store
                .db
                .query_row("SELECT json FROM result_sets WHERE key=?1", [&key], |r| {
                    r.get(0)
                })
                .optional()?;
            let result = if let Some(saved) = saved {
                serde_json::from_str::<retrieval::SearchResult>(&saved)?
            } else {
                if continuation.is_some() {
                    return Err(Error::new(
                        "stale_continuation",
                        "retained search order is unavailable",
                    ));
                }
                let result = retrieval::search(
                    &self.store,
                    &self.config.search,
                    self.config.vectors.as_ref(),
                    &view,
                    request.query.as_deref().unwrap_or(""),
                )?;
                self.store.db.execute(
                    "INSERT INTO result_sets VALUES(?1,?2,?3)",
                    params![key, view.id, serde_json::to_string(&result)?],
                )?;
                result
            };
            envelope["coverage"]["query_relaxed"] = json!(result.query_relaxed);
            envelope["coverage"]["vectors"] = result.vectors["status"].clone();
            envelope["coverage"]["vector_details"] = result.vectors;
            envelope["coverage"]["expansion"] = result.expansion;
            result
                .candidates
                .into_iter()
                .filter(|c| {
                    !tombstone::excludes_record(
                        &self.tombstones,
                        &forgotten_ids,
                        &c.member.id,
                        &c.member.source_id,
                        c.member.observation.recorded_at.as_deref(),
                    )
                })
                .collect()
        };
        // Availability is sampled once per source in this read, never retained
        // across requests. Historical/current classification remains per member.
        let mut availability = AvailabilityCache::new();
        if operation == "brief" && request.compact {
            if continuation
                .as_ref()
                .is_some_and(|c| c.stream != "brief-evidence")
            {
                return Err(Error::new(
                    "stale_continuation",
                    "wrong continuation stream",
                ));
            }
            let mut selected = Vec::with_capacity(candidates.len());
            for candidate in &candidates {
                let text = self
                    .store
                    .payload(&candidate.member.version_id)?
                    .redacted_text;
                let mut item =
                    self.item_value(&candidate.member, "", None, &view, &mut availability)?;
                compact_item(&mut item);
                selected.push(Evidence {
                    item,
                    text,
                    id: candidate.member.id.clone(),
                    required: candidate.member.required,
                });
            }
            return self.evidence_page(
                "brief",
                selected,
                &budget,
                envelope,
                continuation.as_ref().map_or(0, |c| c.offset),
                &token,
            );
        }
        let start = continuation.as_ref().map_or(0, |c| c.offset);
        if start > candidates.len() {
            return Err(Error::new("stale_continuation", "offset is out of range"));
        }
        let stream = continuation
            .as_ref()
            .map(|c| c.stream.as_str())
            .unwrap_or("results");
        if !matches!(stream, "results" | "omitted")
            || (operation == "search" && stream != "results")
        {
            return Err(Error::new(
                "stale_continuation",
                "wrong continuation stream",
            ));
        }
        if stream == "omitted" {
            envelope["incomplete"] = json!(true);
            envelope["reason"] = json!(if candidates[start..].iter().any(|c| c.member.required) {
                "required_items_omitted"
            } else {
                "results_omitted"
            });
            envelope["omitted_count"] = json!(candidates.len() - start);
            envelope["continuation"] = if start < candidates.len() {
                token("omitted", start)?
            } else {
                Value::Null
            };
            budget.require(&mut envelope)?;
            let mut next = start;
            for (i, c) in candidates.iter().enumerate().skip(start) {
                let mut candidate = envelope.clone();
                candidate["omitted"]
                    .as_array_mut()
                    .expect("array")
                    .push(omission(&c.member, request.compact));
                candidate["continuation"] = if i + 1 < candidates.len() {
                    token("omitted", i + 1)?
                } else {
                    Value::Null
                };
                if budget.settle(&mut candidate)? > budget.limit() {
                    break;
                }
                envelope = candidate;
                next = i + 1;
            }
            if next == start && start < candidates.len() {
                let mut minimum = envelope.clone();
                minimum["omitted"] = json!([omission(&candidates[start].member, request.compact)]);
                let needed = budget.settle(&mut minimum)?;
                return Err(Error::new(
                    "budget_below_minimum",
                    json!({"minimum_budget":needed,"reason":"no pagination progress"}),
                ));
            }
            budget.require(&mut envelope)?;
            return budget.finish(envelope);
        }
        let limit = if operation == "search" {
            self.config.search.result_limit
        } else {
            usize::MAX
        };
        let mut next = start;
        let mut first_result_minimum = None;
        // Reserve the actual omission metadata and continuation before adding text.
        set_omissions(&mut envelope, &candidates, next, operation, &token)?;
        budget.require(&mut envelope)?;
        for (i, c) in candidates.iter().enumerate().skip(start).take(limit) {
            let mut candidate = envelope.clone();
            let text = self.store.payload(&c.member.version_id)?.redacted_text;
            let text = if operation == "search" {
                let end = utf8_end(&text, 600);
                text[..end].to_owned()
            } else {
                text
            };
            let mut item = self.item_value(&c.member, &text, None, &view, &mut availability)?;
            item["paths_found"] = json!(c.paths);
            if let Some(semantic) = &c.semantic
                && let Some(relations) = item.get_mut("relations").and_then(Value::as_array_mut)
            {
                relations.push(semantic.clone());
            }
            if request.compact {
                compact_item(&mut item);
            }
            candidate["items"].as_array_mut().expect("array").push(item);
            set_omissions(&mut candidate, &candidates, i + 1, operation, &token)?;
            let used = budget.settle(&mut candidate)?;
            if i == start {
                first_result_minimum = Some(used);
            }
            if used > budget.limit() {
                break;
            }
            envelope = candidate;
            next = i + 1;
        }
        if operation == "brief" {
            for (i, c) in candidates.iter().enumerate().skip(next) {
                let mut candidate = envelope.clone();
                candidate["omitted"]
                    .as_array_mut()
                    .expect("array")
                    .push(omission(&c.member, request.compact));
                candidate["continuation"] = if i + 1 < candidates.len() {
                    token("omitted", i + 1)?
                } else {
                    Value::Null
                };
                if budget.settle(&mut candidate)? > budget.limit() {
                    break;
                }
                envelope = candidate;
            }
        }
        // Exact matches beyond a result page remain available through its cursor.
        if operation == "search" {
            envelope["coverage"]["truncated_exact"] = json!(
                candidates[next..]
                    .iter()
                    .any(|c| c.paths.iter().any(|p| p == "exact"))
            );
            if next == start && start < candidates.len() {
                return Err(Error::new(
                    "budget_below_minimum",
                    json!({"reason":"search result cannot fit","minimum_budget":first_result_minimum}),
                ));
            }
        }
        budget.require(&mut envelope)?;
        budget.finish(envelope)
    }

    fn validate_view_scope(&self, view: &View, request: &ReadRequest) -> Result<()> {
        let mut branches = request.branches.clone();
        branches.sort();
        branches.dedup();
        if view.meta["scope"]["worktree"] != self.repo.root.to_string_lossy().as_ref()
            || view.meta["scope"]["project_id"] != self.config.project_id
            || view.meta["scope"]["branches"] != json!(branches)
            || request
                .task
                .as_ref()
                .is_some_and(|t| view.meta["scope"]["task"] != *t)
            || request
                .incoming
                .is_some_and(|v| view.meta["scope"]["incoming"] != v)
            || (branches.is_empty() && view.meta["scope"]["branch"] != self.repo.state()?.branch)
        {
            return Err(Error::new(
                "stale_continuation",
                "view belongs to another scope",
            ));
        }
        Ok(())
    }

    fn envelope(&self, operation: &str, view: &View, compact: bool) -> Result<Value> {
        let mut freshness = view.meta["freshness"].clone();
        if freshness["status"] == "current"
            && self
                .store
                .current(&view.scope_hash)?
                .is_none_or(|v| v.id != view.id)
        {
            freshness["status"] = json!("stale");
            freshness["reason"] = json!("retained_view");
        }
        Ok(presentation::envelope(
            operation, &view.meta, freshness, compact,
        ))
    }

    fn item_value(
        &self,
        m: &Member,
        text: &str,
        historical: Option<&str>,
        view: &View,
        availability: &mut AvailabilityCache,
    ) -> Result<Value> {
        if self.store.is_forgotten(&m.id)?
            || tombstone::excludes_id(
                &self.tombstones,
                &m.id,
                m.observation.recorded_at.as_deref(),
            )
        {
            return Ok(presentation::unavailable(&m.id, "forgotten"));
        }
        let availability = if m.observation.origin == "incoming" {
            "incoming"
        } else {
            let source = if m.kind.starts_with("harness-") {
                AvailabilitySource::Harness {
                    root: m.observation.source_root.clone(),
                    path: m.observation.source_path.clone(),
                }
            } else {
                AvailabilitySource::Repository {
                    root: self.repo.root.clone(),
                    path: m.observation.source_path.clone(),
                }
            };
            let source_available =
                *availability
                    .entry(source)
                    .or_insert_with_key(|source| match source {
                        AvailabilitySource::Repository { path, .. } => {
                            self.repo.read_at(path, None).is_ok_and(|v| v.is_some())
                        }
                        AvailabilitySource::Harness { root, path } => root
                            .as_ref()
                            .is_some_and(|root| std::path::Path::new(root).join(path).exists()),
                    });
            if !source_available {
                "source_missing"
            } else if historical.is_some()
                || (view.meta["scope"]["branches"]
                    .as_array()
                    .is_none_or(Vec::is_empty)
                    && view.meta["scope"]["head"] != json!(m.observation.commit))
            {
                "historical"
            } else {
                "current"
            }
        };
        let revision_scoped = m.observation.origin == "incoming"
            || view.meta["scope"]["branches"]
                .as_array()
                .is_some_and(|branches| !branches.is_empty());
        let verification = m.verification.as_ref().map(|report| {
            // Live checkout reports follow current working files. Combined
            // branch and incoming members stay bound to their saved Git object.
            let revision = revision_scoped
                .then_some(m.observation.commit.as_deref())
                .flatten();
            let mut evaluated = crate::verification::evaluate(&self.repo, report, revision);
            if revision_scoped && revision.is_none() {
                evaluated["applicability"] = json!("unknown");
            }
            evaluated
        });
        Ok(presentation::item(
            m,
            text,
            availability,
            historical,
            &view.meta,
            verification,
        ))
    }

    fn show_page(
        &self,
        view: &View,
        request: &ReadRequest,
        budget: &ReplyBudget,
        envelope: Value,
        offset: usize,
        token: &impl Fn(&str, usize) -> Result<Value>,
    ) -> Result<Value> {
        let mut selected = Vec::<Evidence>::new();
        let mut availability = AvailabilityCache::new();
        for id in &request.ids {
            if self.store.is_forgotten(id)? || tombstone::excludes_id(&self.tombstones, id, None) {
                selected.push(Evidence {
                    item: presentation::unavailable(id, "forgotten"),
                    text: String::new(),
                    id: id.clone(),
                    required: false,
                });
                continue;
            }
            let found: Vec<_> = view.members.iter().filter(|m| &m.id == id).collect();
            if !found.is_empty() {
                for member in found {
                    let payload = self.store.payload(&member.version_id)?;
                    let item = self.item_value(member, "", None, view, &mut availability)?;
                    selected.push(Evidence {
                        item,
                        text: payload.redacted_text,
                        id: id.clone(),
                        required: member.required,
                    });
                }
                continue;
            }
            let mut old = None;
            for (member, old_view, meta) in self.store.history_at(id, &view.id)? {
                let branch_ok = if request.branches.is_empty() {
                    (json!(member.observation.branch) == view.meta["scope"]["branch"]
                        || member.observation.branch == "unknown")
                        && meta["scope"]["branch"] == view.meta["scope"]["branch"]
                } else {
                    request.branches.contains(&member.observation.branch)
                };
                if member.observation.worktree_key == self.repo.root.to_string_lossy()
                    && branch_ok
                    && (member.observation.origin == "local" || request.incoming.unwrap_or(false))
                {
                    old = Some((member, old_view));
                    break;
                }
            }
            if let Some((member, old_view)) = old {
                if tombstone::excludes_id(
                    &self.tombstones,
                    id,
                    member.observation.recorded_at.as_deref(),
                ) {
                    selected.push(Evidence {
                        item: presentation::unavailable(id, "forgotten"),
                        text: String::new(),
                        id: id.clone(),
                        required: false,
                    });
                    continue;
                }
                let payload = self.store.payload(&member.version_id)?;
                selected.push(Evidence {
                    item: self.item_value(&member, "", Some(&old_view), view, &mut availability)?,
                    text: payload.redacted_text,
                    id: id.clone(),
                    required: member.required,
                });
            } else {
                selected.push(Evidence {
                    item: presentation::unavailable(id, "not_found_in_scope"),
                    text: String::new(),
                    id: id.clone(),
                    required: false,
                });
            }
        }
        if request.compact {
            for evidence in &mut selected {
                compact_item(&mut evidence.item);
            }
        }
        self.evidence_page("show", selected, budget, envelope, offset, token)
    }

    fn evidence_page(
        &self,
        operation: &str,
        selected: Vec<Evidence>,
        budget: &ReplyBudget,
        mut envelope: Value,
        offset: usize,
        token: &impl Fn(&str, usize) -> Result<Value>,
    ) -> Result<Value> {
        let total: usize = selected.iter().map(|e| e.text.len() + 1).sum();
        if total == 0 && offset == 0 {
            return budget.finish(envelope);
        }
        if offset >= total {
            return Err(Error::new(
                "stale_continuation",
                "evidence offset is out of range",
            ));
        }
        // A complete page loses cursor overhead and may share repeated bodies.
        // Skip that attempt only when bytes that must survive in the output
        // prove it cannot fit. Raw per-occurrence text is not such a bound.
        let mut unique_bodies = std::collections::HashSet::new();
        let mut minimum_bytes = 0usize;
        let mut base = 0;
        for Evidence { text, id, .. } in &selected {
            let end = base + text.len() + 1;
            if offset < end {
                let start = offset.saturating_sub(base);
                if !text.is_char_boundary(start) || (!text.is_empty() && start == text.len()) {
                    return Err(Error::new(
                        "stale_continuation",
                        "invalid evidence boundary",
                    ));
                }
                if unique_bodies.insert((id.as_str(), &text[start..])) {
                    minimum_bytes = minimum_bytes
                        .saturating_add(id.len())
                        .saturating_add(text.len() - start);
                }
                if !budget.may_fit_bytes(minimum_bytes) {
                    break;
                }
            }
            base = end;
        }
        let try_complete = budget.may_fit_bytes(minimum_bytes);
        drop(unique_bodies);
        if try_complete {
            let mut complete = envelope.clone();
            let mut all_ids = Vec::new();
            let mut base = 0;
            for Evidence { item, text, id, .. } in &selected {
                let end = base + text.len() + 1;
                if offset < end {
                    let start = offset.saturating_sub(base);
                    if !text.is_char_boundary(start) || (!text.is_empty() && start == text.len()) {
                        return Err(Error::new(
                            "stale_continuation",
                            "invalid evidence boundary",
                        ));
                    }
                    complete["items"]
                        .as_array_mut()
                        .expect("array")
                        .push(render(
                            item,
                            text,
                            start,
                            text.len(),
                            operation == "show" || start != 0,
                        ));
                    if item.get("text").is_some() && !all_ids.contains(id) {
                        all_ids.push(id.clone());
                    }
                }
                base = end;
            }
            complete["coverage"]["truncated"] = json!(false);
            if budget.settle(&mut complete)? <= budget.limit() {
                if operation == "show" {
                    self.store.touch(&all_ids)?;
                }
                return budget.finish(complete);
            }
        }

        let mut required_after = vec![false; selected.len() + 1];
        for i in (0..selected.len()).rev() {
            required_after[i] = selected[i].required || required_after[i + 1];
        }
        let member_count = selected.len();
        let stream = if operation == "brief" {
            "brief-evidence"
        } else {
            "evidence"
        };
        let initially_incomplete = envelope["incomplete"] == true;
        let initial_reason = envelope["reason"].clone();
        let mut served = Vec::new();
        let mut base = 0;
        for (index, Evidence { item, text, id, .. }) in selected.into_iter().enumerate() {
            let end = base + text.len() + 1;
            if offset >= end {
                base = end;
                continue;
            }
            let start = offset.saturating_sub(base);
            if !text.is_char_boundary(start) || (!text.is_empty() && start == text.len()) {
                return Err(Error::new(
                    "stale_continuation",
                    "invalid evidence boundary",
                ));
            }
            let make = |stop: usize| -> Result<Value> {
                let next = if stop == text.len() { end } else { base + stop };
                let mut out = envelope.clone();
                out["items"].as_array_mut().expect("array").push(render(
                    &item,
                    &text,
                    start,
                    stop,
                    operation == "show" || start != 0 || stop != text.len(),
                ));
                out["continuation"] = if next < total {
                    token(stream, next)?
                } else {
                    Value::Null
                };
                out["coverage"]["truncated"] = json!(next < total);
                out["incomplete"] = json!(next < total || initially_incomplete);
                out["reason"] = if next < total && operation == "brief" {
                    let remaining = index + usize::from(stop == text.len());
                    json!(if required_after[remaining] {
                        "required_items_omitted"
                    } else {
                        "results_omitted"
                    })
                } else if next < total {
                    json!("evidence_page")
                } else {
                    initial_reason.clone()
                };
                if operation == "brief" {
                    out["omitted_count"] =
                        json!(member_count - index - usize::from(stop == text.len()));
                }
                Ok(out)
            };
            let mut full = make(text.len())?;
            if budget.settle(&mut full)? <= budget.limit() {
                envelope = full;
                if item.get("text").is_some() && !served.contains(&id) {
                    served.push(id);
                }
                base = end;
                continue;
            }
            // A partial member must contain a complete code point. If it cannot
            // fit after earlier members, leave it untouched for the next page.
            let first = start + text[start..].chars().next().map_or(0, char::len_utf8);
            let mut minimum = make(first)?;
            let needed = budget.settle(&mut minimum)?;
            if needed > budget.limit() {
                if envelope["items"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    break;
                }
                return Err(Error::new(
                    "budget_below_minimum",
                    json!({"minimum_budget":needed,"reason":"no evidence pagination progress"}),
                ));
            }
            let mut lo = first;
            let mut hi = utf8_end(
                &text,
                start
                    .saturating_add(budget.limit().saturating_mul(8))
                    .min(text.len()),
            );
            let mut best = minimum;
            while lo <= hi {
                let mid = utf8_end(&text, lo + (hi - lo) / 2);
                let mut out = make(mid)?;
                if budget.settle(&mut out)? <= budget.limit() {
                    best = out;
                    lo = mid + 1;
                    while lo < text.len() && !text.is_char_boundary(lo) {
                        lo += 1;
                    }
                } else {
                    if mid == first {
                        break;
                    }
                    hi = mid - 1;
                }
            }
            envelope = best;
            if item.get("text").is_some() && !served.contains(&id) {
                served.push(id);
            }
            break;
        }
        budget.require(&mut envelope)?;
        if operation == "show" {
            self.store.touch(&served)?;
        }
        budget.finish(envelope)
    }
}

fn set_omissions(
    envelope: &mut Value,
    candidates: &[Candidate],
    next: usize,
    operation: &str,
    token: &impl Fn(&str, usize) -> Result<Value>,
) -> Result<()> {
    let remaining = &candidates[next..];
    let required = remaining.iter().any(|c| c.member.required);
    envelope["omitted_count"] = json!(remaining.len());
    envelope["coverage"]["truncated"] = json!(!remaining.is_empty());
    if !remaining.is_empty() {
        envelope["incomplete"] = json!(true);
        envelope["reason"] = json!(if required && operation == "brief" {
            "required_items_omitted"
        } else {
            "results_omitted"
        });
        envelope["continuation"] = token(
            if operation == "brief" {
                "omitted"
            } else {
                "results"
            },
            next,
        )?;
    } else {
        envelope["continuation"] = Value::Null;
        envelope["incomplete"] = json!(envelope["freshness"]["status"] != "current");
        envelope["reason"] = envelope["freshness"]["reason"].clone();
    }
    Ok(())
}
