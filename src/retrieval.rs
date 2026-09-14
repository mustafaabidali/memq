use crate::config::{SearchConfig, VectorConfig};
use crate::error::{Error, Result};
use crate::policy::Member;
use crate::records;
use crate::store::{Store, View};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    pub member: Member,
    pub paths: Vec<String>,
    #[serde(default)]
    pub semantic: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchResult {
    pub candidates: Vec<Candidate>,
    pub query_relaxed: bool,
    pub vectors: serde_json::Value,
    pub expansion: serde_json::Value,
}

pub fn literal_terms(query: &str) -> Vec<String> {
    records::normalized(query)
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn fts_query(terms: &[String], join: &str) -> String {
    terms
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(join)
}

pub(crate) fn search(
    store: &Store,
    config: &SearchConfig,
    vectors: Option<&VectorConfig>,
    view: &View,
    query: &str,
) -> Result<SearchResult> {
    if query.trim().is_empty() || query.len() > 16_384 {
        return Err(Error::new(
            "query_error",
            "search text must contain 1 to 16384 bytes",
        ));
    }
    #[cfg(debug_assertions)]
    if std::env::var("MEMQ_FAULT").as_deref() == Ok("search_engine") {
        store
            .db
            .execute_batch("SELECT controlled_missing_search_engine();")?;
    }
    let mut candidates = BTreeMap::<(String, String, String), Candidate>::new();
    let key = |m: &Member| {
        (
            m.id.clone(),
            m.observation.origin.clone(),
            m.observation.branch.clone(),
        )
    };
    let literal_versions = if query.chars().count() >= 3 {
        store.with_view_db(&view.id, |db| {
            // Trigram candidates are filtered by scope before materialization.
            // Exact candidates are deliberately not capped by a lexical limit.
            let mut stmt = db.prepare(
                "SELECT v.version_id FROM items_trigram
                 JOIN item_versions v ON v.vrow=items_trigram.rowid
                 WHERE items_trigram MATCH ?1 AND EXISTS(
                     SELECT 1 FROM view_items vi WHERE vi.view_id=?2 AND vi.version_id=v.version_id
                 )",
            )?;
            let rows = stmt.query_map(
                params![format!("\"{}\"", query.replace('"', "\"\"")), view.id],
                |r| r.get::<_, String>(0),
            )?;
            Ok(rows.collect::<std::result::Result<BTreeSet<_>, _>>()?)
        })?
    } else {
        // FTS5 cannot represent a substring shorter than three code points.
        view.members.iter().map(|m| m.version_id.clone()).collect()
    };
    for m in &view.members {
        let exact = m.id == query
            || m.native_id == query
            || m.observation.source_path == query
            || (literal_versions.contains(&m.version_id)
                && store.payload(&m.version_id)?.redacted_text.contains(query));
        if exact {
            candidates.insert(
                key(m),
                Candidate {
                    member: m.clone(),
                    paths: vec!["exact".into()],
                    semantic: None,
                },
            );
        }
    }
    let terms = literal_terms(query);
    let mut relaxed = false;
    let mut lexical = Vec::new();
    if !terms.is_empty() {
        lexical = fts(
            store,
            config.candidate_limit,
            view,
            &fts_query(&terms, " AND "),
        )?;
        if lexical.is_empty() && terms.len() > 1 {
            lexical = fts(
                store,
                config.candidate_limit,
                view,
                &fts_query(&terms, " OR "),
            )?;
            relaxed = true;
        }
    }
    let mut positions = BTreeMap::new();
    for (rank, version) in lexical.iter().enumerate() {
        for m in view.members.iter().filter(|m| &m.version_id == version) {
            let k = key(m);
            positions.insert(k.clone(), rank);
            candidates
                .entry(k)
                .or_insert_with(|| Candidate {
                    member: m.clone(),
                    paths: vec![],
                    semantic: None,
                })
                .paths
                .push("fts".into());
        }
    }
    let (vector_candidates, vector_coverage) =
        crate::vectors::search(store, vectors, config.candidate_limit, view, query)?;
    let mut vector_positions = BTreeMap::new();
    for (rank, vector) in vector_candidates.iter().enumerate() {
        for m in view
            .members
            .iter()
            .filter(|m| m.version_id == vector.version_id)
        {
            let k = key(m);
            vector_positions.insert(k.clone(), rank);
            let candidate = candidates.entry(k).or_insert_with(|| Candidate {
                member: m.clone(),
                paths: vec![],
                semantic: None,
            });
            candidate.paths.push("vector".into());
            candidate.semantic = Some(vector.provenance.clone());
        }
    }
    let mut out: Vec<_> = candidates.into_values().collect();
    out.sort_by(|a, b| {
        let exact = |c: &Candidate| {
            if c.member.id == query || c.member.native_id == query {
                0
            } else if c.member.observation.source_path == query {
                1
            } else if c.paths.iter().any(|s| s == "exact") {
                2
            } else {
                3
            }
        };
        exact(a)
            .cmp(&exact(b))
            .then_with(|| {
                let score = |c: &Candidate| {
                    [&positions, &vector_positions]
                        .iter()
                        .filter_map(|p| p.get(&key(&c.member)))
                        .map(|rank| 1.0 / (config.fusion_k + rank + 1) as f64)
                        .sum::<f64>()
                };
                score(b).total_cmp(&score(a))
            })
            .then(a.member.id.cmp(&b.member.id))
            .then(
                a.member
                    .observation
                    .branch
                    .cmp(&b.member.observation.branch),
            )
    });
    let original = out.clone();
    let mut expanded = 0;
    let mut missing = 0;
    for candidate in original.iter().take(config.result_limit) {
        for relation in &candidate.member.relations {
            if relation["status"] != "resolved" {
                missing += 1;
                continue;
            }
            if expanded >= config.candidate_limit {
                break;
            }
            if let Some(member) = view.members.iter().find(|m| {
                json!(m.id) == relation["to"]
                    && json!(m.version_id) == relation["to_version_id"]
                    && m.observation.origin == candidate.member.observation.origin
                    && m.observation.branch == candidate.member.observation.branch
            }) && !out.iter().any(|c| key(&c.member) == key(member))
            {
                out.push(Candidate {
                    member: member.clone(),
                    paths: vec!["authored".into()],
                    semantic: None,
                });
                expanded += 1;
            }
        }
    }
    Ok(SearchResult {
        candidates: out,
        query_relaxed: relaxed,
        vectors: vector_coverage,
        expansion: json!({"hops":1,"expanded":expanded,"limit":config.candidate_limit,"missing_endpoints":missing}),
    })
}

fn fts(store: &Store, candidate_limit: usize, view: &View, query: &str) -> Result<Vec<String>> {
    // Eligibility is applied inside SQL, before LIMIT. Historical versions in
    // other worktrees still affect BM25 statistics, never candidate membership.
    store.with_view_db(&view.id, |db| {
        let mut stmt = db.prepare(
            "SELECT v.version_id FROM items_fts
         JOIN item_versions v ON v.vrow=items_fts.rowid
         WHERE items_fts MATCH ?1 AND EXISTS(
             SELECT 1 FROM view_items vi WHERE vi.view_id=?2 AND vi.version_id=v.version_id
         ) ORDER BY bm25(items_fts),v.version_id LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(params![query, view.id, candidate_limit as i64], |r| {
                r.get(0)
            })
            .map_err(|e| Error::new("search_engine_error", json!({"message":e.to_string()})))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    })
}
