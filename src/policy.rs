use crate::config::Policy;
use crate::records::{Item, Observation, field_string, field_values};
pub use crate::verification::Report as VerificationReport;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Member {
    pub id: String,
    pub version_id: String,
    pub source_id: String,
    pub native_id: String,
    pub kind: String,
    pub native_status: Option<String>,
    pub acceptance: String,
    pub attribution: String,
    pub flags: Vec<String>,
    pub conflict_with: Vec<String>,
    pub supersession: Option<String>,
    pub reason: Option<String>,
    pub section: String,
    pub rank: usize,
    pub observation: Observation,
    pub relations: Vec<Value>,
    #[serde(
        default,
        deserialize_with = "crate::verification::deserialize_optional"
    )]
    pub verification: Option<VerificationReport>,
    pub task: Option<String>,
    pub required: bool,
    #[serde(default)]
    pub replacements: Vec<String>,
    #[serde(default)]
    pub replaced_by: Vec<String>,
}

pub fn evaluate(item: &Item, policy: &Policy, references: &[String]) -> Member {
    let status = item.payload.native_status.as_deref().unwrap_or("");
    let base = if policy.accepted_values.iter().any(|s| s == status) {
        "accepted"
    } else if policy.proposed_values.iter().any(|s| s == status) {
        "proposed"
    } else if policy.withdrawn_values.iter().any(|s| s == status) {
        "withdrawn"
    } else if policy.superseded_values.iter().any(|s| s == status) {
        "superseded"
    } else if policy.accepted_values.is_empty()
        && policy.proposed_values.is_empty()
        && policy.withdrawn_values.is_empty()
        && policy.superseded_values.is_empty()
    {
        "n/a"
    } else {
        "unknown"
    };
    let record = &item.payload.record;
    let attribution =
        field_string(record, policy.decider_field.as_deref()).filter(|s| !s.is_empty());
    let mut acceptance = base.to_owned();
    let mut flags = Vec::new();
    if let Some(approvers) = &policy.approvers {
        match &attribution {
            None if policy.require_attribution => {
                acceptance = "unverified".into();
                flags.push("attribution_required_missing".into());
            }
            None => flags.push("attribution_missing".into()),
            Some(who) if !approvers.contains(who) => {
                acceptance = "unverified".into();
                flags.push("approver_not_listed".into());
            }
            _ => (),
        }
    }
    let blocked = policy.blocked_values.iter().any(|s| s == status);
    let active = policy.active_values.iter().any(|s| s == status);
    let constraint = record["kind"] == "constraint";
    let section = if blocked {
        "blockers"
    } else if acceptance == "accepted" || constraint {
        "accepted_decisions"
    } else if matches!(acceptance.as_str(), "proposed" | "unverified") {
        "pending_decisions"
    } else if item.payload.kind == "memq-notes" {
        "progress"
    } else if active {
        "next_actions"
    } else {
        "evidence"
    };
    let mut fields = references.to_vec();
    if item.payload.kind == "memq-notes" {
        fields.push("evidence".into());
    }
    for f in [&policy.supersedes_field, &policy.superseded_by_field]
        .into_iter()
        .flatten()
    {
        if !fields.contains(f) {
            fields.push(f.clone());
        }
    }
    let relations = fields
        .into_iter()
        .flat_map(|field| {
            field_values(record, &field).into_iter().map(move |target| {
                json!({
                    "kind": "authored", "field": field, "to":target,
                    "from_version_id":item.version_id, "to_version_id":null,
                    "status":"missing_endpoint"
                })
            })
        })
        .collect();
    let verification = record
        .get("verification")
        .filter(|value| !value.is_null())
        .and_then(|value| {
            let report = VerificationReport::parse(value);
            if report.is_none() {
                flags.push("unsupported_verification_schema".into());
            }
            report
        });
    Member {
        id: item.id.clone(),
        version_id: item.version_id.clone(),
        source_id: item.source_id.clone(),
        native_id: item.payload.native_id.clone(),
        kind: item.payload.kind.clone(),
        native_status: item.payload.native_status.clone(),
        acceptance,
        attribution: attribution.unwrap_or_else(|| "missing".into()),
        flags,
        conflict_with: vec![],
        supersession: None,
        reason: field_string(record, policy.reason_field.as_deref()),
        section: section.into(),
        rank: 0,
        observation: item.observation.clone(),
        relations,
        verification,
        task: record
            .pointer("/scope/task")
            .or_else(|| record.get("task"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        required: blocked
            || constraint
            || matches!(section, "accepted_decisions" | "pending_decisions"),
        replacements: policy
            .supersedes_field
            .as_ref()
            .map(|f| field_values(record, f))
            .unwrap_or_default(),
        replaced_by: policy
            .superseded_by_field
            .as_ref()
            .map(|f| field_values(record, f))
            .unwrap_or_default(),
    }
}

fn names(target: &str, m: &Member) -> bool {
    target == m.id
        || target == m.native_id
        || target.split([',', ';']).any(|t| {
            t.trim() == m.native_id
                || t.trim()
                    .strip_prefix(&m.native_id)
                    .is_some_and(|rest| rest.starts_with(" ("))
        })
}

fn same_scope(a: &Member, b: &Member) -> bool {
    a.observation.branch == b.observation.branch && a.observation.origin == b.observation.origin
}

pub fn reconcile(members: &mut [Member]) {
    let original = members.to_vec();
    let mut scopes = BTreeMap::new();
    let mut incoming_ids = BTreeMap::new();
    let mut references = BTreeSet::new();
    for (index, member) in original.iter().enumerate() {
        let scope = (
            member.observation.branch.as_str(),
            member.observation.origin.as_str(),
        );
        scopes.entry(scope).or_insert_with(Vec::new).push(index);
        if member.observation.origin == "incoming" {
            incoming_ids
                .entry(member.id.as_str())
                .or_insert_with(Vec::new)
                .push(index);
        }
        references.extend(
            member
                .replacements
                .iter()
                .chain(&member.replaced_by)
                .map(String::as_str),
        );
        references.extend(
            member
                .relations
                .iter()
                .map(|relation| relation["to"].as_str().unwrap_or("")),
        );
    }

    // Resolve each expression once per scope, preserving the existing native
    // name, qualified ID, annotation, and ambiguous-reference rules.
    let mut resolutions = BTreeMap::new();
    for (&scope, indices) in &scopes {
        for &reference in &references {
            let matches: Vec<usize> = indices
                .iter()
                .copied()
                .filter(|&index| names(reference, &original[index]))
                .collect();
            resolutions.insert((scope, reference), matches);
        }
    }
    let mut replacement_candidates = vec![BTreeSet::new(); original.len()];
    let mut replacement_targets = vec![BTreeSet::new(); original.len()];
    for (index, member) in original.iter().enumerate() {
        let own_scope = (
            member.observation.branch.as_str(),
            member.observation.origin.as_str(),
        );
        if !member.replacements.is_empty() {
            for &scope in scopes.keys() {
                if scope != own_scope
                    && !(member.observation.origin == "incoming" && scope.1 == "local")
                {
                    continue;
                }
                for reference in &member.replacements {
                    if let [target] = resolutions[&(scope, reference.as_str())].as_slice() {
                        replacement_candidates[*target].insert(index);
                        if scope == own_scope {
                            replacement_targets[index].insert(original[*target].id.clone());
                        }
                    }
                }
            }
        }
        if !member.replaced_by.is_empty() {
            for &scope in scopes.keys() {
                if scope != own_scope
                    && !(member.observation.origin == "local" && scope.1 == "incoming")
                {
                    continue;
                }
                for reference in &member.replaced_by {
                    if let [replacement] = resolutions[&(scope, reference.as_str())].as_slice() {
                        replacement_candidates[index].insert(*replacement);
                    }
                }
            }
        }
        if member.observation.origin == "local"
            && let Some(incoming) = incoming_ids.get(member.id.as_str())
        {
            replacement_candidates[index].extend(incoming.iter().copied());
        }
    }

    for (idx, target) in original.iter().enumerate() {
        let mut accepted_replacements = BTreeSet::new();
        // Index order preserves the previous deterministic flag/endpoint order
        // when more than one authored path reaches the same replacement.
        for &replacement_index in &replacement_candidates[idx] {
            let replacement = &original[replacement_index];
            if target.id == replacement.id && target.version_id == replacement.version_id {
                continue;
            }
            let same_scope = same_scope(target, replacement);
            let upstream = replacement.observation.origin == "incoming"
                && target.observation.origin == "local";
            let same_upstream = upstream && target.id == replacement.id;
            if same_upstream {
                if replacement.acceptance == "accepted"
                    && matches!(target.acceptance.as_str(), "proposed" | "unverified")
                {
                    accepted_replacements.insert(replacement.id.clone());
                    // This is a resolved pending version, not a decision
                    // superseding its own logical identity.
                    members[idx].supersession = None;
                    members[idx].section = "resolved_upstream_not_local".into();
                    members[idx].required = true;
                }
                continue;
            }
            if replacement.acceptance == "accepted" {
                accepted_replacements.insert(replacement.id.clone());
                members[idx].supersession = Some(replacement.id.clone());
                members[idx].section = if upstream {
                    "resolved_upstream_not_local"
                } else {
                    "superseded"
                }
                .into();
                members[idx].required = upstream;
            } else if same_scope {
                members[idx]
                    .flags
                    .push(format!("proposed_replacement: {}", replacement.id));
            }
        }
        if accepted_replacements.len() > 1 {
            members[idx].supersession = None;
            members[idx].section = target.section.clone();
            members[idx].required = true;
            members[idx].flags.push("supersession_conflict".into());
            members[idx].conflict_with.extend(accepted_replacements);
        }
        let mut relations = target.relations.clone();
        for relation in &mut relations {
            let to = relation["to"].as_str().unwrap_or("");
            let scope = (
                target.observation.branch.as_str(),
                target.observation.origin.as_str(),
            );
            let matches = &resolutions[&(scope, to)];
            if let [endpoint] = matches.as_slice() {
                let endpoint = *endpoint;
                relation["to"] = json!(original[endpoint].id);
                relation["to_version_id"] = json!(original[endpoint].version_id);
                relation["status"] = json!("resolved");
            } else if matches.len() > 1 {
                relation["status"] = json!("ambiguous_endpoint");
            }
        }
        members[idx].relations = relations;
    }
    let accepted: Vec<usize> = original
        .iter()
        .enumerate()
        .filter_map(|(index, member)| (member.acceptance == "accepted").then_some(index))
        .collect();
    for (position, &i) in accepted.iter().enumerate() {
        for &j in &accepted[position + 1..] {
            let a = &original[i];
            let b = &original[j];
            let same_native_across_branches = a.native_id == b.native_id
                && a.source_id == b.source_id
                && a.observation.branch != b.observation.branch
                && a.version_id != b.version_id
                && a.observation.origin == b.observation.origin;
            let targets_a = &replacement_targets[i];
            let targets_b = &replacement_targets[j];
            let competing = !targets_a.is_disjoint(targets_b)
                && a.id != b.id
                && a.observation.origin == b.observation.origin;
            if (same_native_across_branches || competing)
                && !targets_a.contains(&b.id)
                && !targets_b.contains(&a.id)
            {
                for (left, right) in [(i, j), (j, i)] {
                    members[left].flags.push("conflict".into());
                    members[left].conflict_with.push(original[right].id.clone());
                    members[left].required = true;
                }
            }
        }
    }
}
