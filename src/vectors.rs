use crate::config::VectorConfig;
use crate::error::{Error, Result};
use crate::policy::Member;
use crate::process;
use crate::store::{Store, View};
use crate::util;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use std::time::Duration;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Embeddings {
    model: String,
    dimensions: usize,
    preprocessing_version: String,
    pub(crate) vectors: Vec<Vec<f32>>,
    pub(crate) truncated: Vec<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VectorCandidate {
    pub version_id: String,
    pub provenance: Value,
}

struct Prepared {
    vectors: Vec<(String, u64, Vec<f32>)>,
    coverage: Value,
}

pub(crate) fn embed(config: &VectorConfig, texts: &[String], kind: &str) -> Result<Embeddings> {
    let mut command = Command::new(&config.command[0]);
    command.args(&config.command[1..]);
    let input = util::canonical(&json!({
        "model":config.model,"dimensions":config.dims,"preprocessing_version":config.preprocessing_version,
        "kind":kind,"texts":texts.iter().map(|s|crate::redact::text(s)).collect::<Vec<_>>()
    }))?;
    let out = process::bounded(
        &mut command,
        input,
        Duration::from_secs(config.timeout_seconds),
        texts
            .len()
            .saturating_mul(config.dims)
            .saturating_mul(32)
            .saturating_add(65536),
    )?;
    if !out.success {
        return Err(Error::new(
            if out.timed_out {
                "embedding_timeout"
            } else {
                "embedding_failed"
            },
            json!({"exit":out.code}),
        ));
    }
    let response: Embeddings = serde_json::from_slice(&out.stdout)
        .map_err(|_| Error::new("embedding_schema", "invalid embedding response"))?;
    if response.model != config.model
        || response.dimensions != config.dims
        || response.preprocessing_version != config.preprocessing_version
        || response.vectors.len() != texts.len()
        || response.truncated.len() != texts.len()
        || response.vectors.iter().any(|v| !valid(v, config.dims))
    {
        return Err(Error::new(
            "embedding_schema",
            "embedding model, dimensions, preprocessing, or vectors did not match the request",
        ));
    }
    Ok(response)
}

fn valid(vector: &[f32], dims: usize) -> bool {
    vector.len() == dims
        && vector.iter().all(|v| v.is_finite())
        && vector
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>()
            > 0.0
}

pub(crate) fn fingerprint(store: &Store) -> Result<String> {
    let mut stmt=store.db.prepare("SELECT version_id,model,dims,preprocessing_version,generation_id,state,vector_json FROM vector_meta ORDER BY version_id")?;
    let rows = stmt.query_map([], |r| {
        Ok(json!([
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            util::hash(r.get::<_, String>(6)?)
        ]))
    })?;
    util::hash_json(&rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn prepare(
    store: &Store,
    config: Option<&VectorConfig>,
    candidate_limit: usize,
    members: &[Member],
) -> Result<Prepared> {
    let Some(config) = config else {
        return Ok(Prepared {
            vectors: vec![],
            coverage: json!({"status":"disabled"}),
        });
    };
    let eligible: BTreeSet<_> = members.iter().map(|m| &m.version_id).collect();
    let mut stmt=store.db.prepare(
        "SELECT v.version_id,v.vrow,vm.vector_json,vm.state FROM item_versions v JOIN vector_meta vm ON vm.version_id=v.version_id
         WHERE vm.model=?1 AND vm.dims=?2 AND vm.preprocessing_version=?3 AND vm.generation_id=?4
         AND v.version_id IN (SELECT value FROM json_each(?5)) ORDER BY v.vrow"
    )?;
    let rows = stmt.query_map(
        params![
            config.model,
            config.dims as i64,
            config.preprocessing_version,
            store.generation,
            serde_json::to_string(&eligible)?
        ],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )?;
    let mut vectors = Vec::new();
    let mut invalid = 0;
    let mut partial = 0;
    for row in rows {
        let (version, key, data, state) = row?;
        match serde_json::from_str::<Vec<f32>>(&data) {
            Ok(v)
                if valid(&v, config.dims)
                    && matches!(state.as_str(), "ready" | "partial_content") =>
            {
                if state == "partial_content" {
                    partial += 1;
                }
                vectors.push((version, key, v));
            }
            _ => invalid += 1,
        }
    }
    let total = eligible.len();
    let coverage = json!({
        "status":if vectors.len()==total && partial==0 && invalid==0 {"ready"}else{"pending"},
        "eligible_versions":total,"indexed_versions":vectors.len(),"partial_content":partial,"invalid_vectors":invalid,
        "scope":"eligible_view_before_candidate_limit","candidate_limit":candidate_limit,
        "model":config.model,"dimensions":config.dims,"preprocessing_version":config.preprocessing_version,
        "generation_id":store.generation
    });
    Ok(Prepared { vectors, coverage })
}

pub(crate) fn coverage(
    store: &Store,
    config: Option<&VectorConfig>,
    candidate_limit: usize,
    members: &[Member],
) -> Result<Value> {
    Ok(prepare(store, config, candidate_limit, members)?.coverage)
}

pub(crate) fn search(
    store: &Store,
    config: Option<&VectorConfig>,
    candidate_limit: usize,
    view: &View,
    query: &str,
) -> Result<(Vec<VectorCandidate>, Value)> {
    let Prepared {
        vectors,
        mut coverage,
    } = prepare(store, config, candidate_limit, &view.members)?;
    let Some(config) = config else {
        return Ok((vec![], coverage));
    };
    if vectors.is_empty() {
        return Ok((vec![], coverage));
    }
    let query = match embed(config, &[query.into()], "query") {
        Ok(v) => v,
        Err(e) => {
            coverage["status"] = json!("pending");
            coverage["error"] = json!(e.code);
            return Ok((vec![], coverage));
        }
    };
    if query.truncated[0] {
        coverage["query_truncated"] = json!(true);
    }
    let options = IndexOptions {
        dimensions: config.dims,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let mut index = Index::new(&options)
        .map_err(|_| Error::new("vector_engine_error", "cannot create USearch index"))?;
    let key = util::hash_json(&json!([
        config,
        store.generation,
        vectors
            .iter()
            .map(|(v, k, x)| json!([v, k, util::hash_json(x).unwrap_or_default()]))
            .collect::<Vec<_>>()
    ]))?;
    let path = store
        .root
        .join("generations")
        .join(&store.generation)
        .join(format!("{key}.usearch"));
    let loaded = path.exists()
        && index
            .load(
                path.to_str()
                    .ok_or_else(|| Error::new("vector_engine_error", "non-UTF-8 storage path"))?,
            )
            .is_ok();
    if !loaded || index.size() != vectors.len() {
        // A failed or mismatched load may leave a partially populated index.
        index = Index::new(&options)
            .map_err(|_| Error::new("vector_engine_error", "cannot recreate USearch index"))?;
        index
            .reserve(vectors.len())
            .map_err(|_| Error::new("vector_engine_error", "cannot reserve USearch capacity"))?;
        for (_, key, v) in &vectors {
            index
                .add(*key, v.as_slice())
                .map_err(|_| Error::new("vector_engine_error", "cannot add version vector"))?;
        }
        let temp = path.with_extension("tmp");
        index
            .save(temp.to_str().expect("validated path"))
            .map_err(|_| Error::new("vector_engine_error", "cannot save USearch index"))?;
        fs::File::open(&temp)?.sync_all()?;
        fs::rename(&temp, &path)?;
        util::sync_dir(path.parent().expect("parent"))?;
    }
    let matches = index
        .search(
            query.vectors[0].as_slice(),
            candidate_limit.min(vectors.len()),
        )
        .map_err(|_| Error::new("vector_engine_error", "USearch candidate retrieval failed"))?;
    let mut out = Vec::new();
    for (key, distance) in matches.keys.iter().zip(&matches.distances) {
        if let Some((version, _, _)) = vectors.iter().find(|(_, k, _)| k == key) {
            out.push(VectorCandidate{
                version_id:version.clone(),
                provenance:json!({"kind":"semantic","model":config.model,"dims":config.dims,"preprocessing_version":config.preprocessing_version,
                    "generation_id":store.generation,"distance":distance,"to_version_id":version})
            });
        }
    }
    coverage["bounded"] = json!(out.len() < vectors.len());
    Ok((out, coverage))
}
