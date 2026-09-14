//! Vector metadata transactions. Refresh and model execution are owned by core.
use super::{Store, View};
use crate::config::VectorConfig;
use crate::error::Result;
use crate::util;
use rusqlite::params;
use std::collections::BTreeSet;

pub(crate) struct Passage {
    pub version_id: String,
    pub text: String,
}

pub(crate) struct VectorUpdate {
    pub version_id: String,
    pub vector: Vec<f32>,
    pub partial: bool,
}

pub(crate) struct VectorPublication {
    pub published: usize,
    pub discarded: usize,
    pub partial: usize,
}

impl Store {
    pub(crate) fn pending_embeddings(
        &self,
        view: &View,
        config: &VectorConfig,
    ) -> Result<Vec<Passage>> {
        let versions: BTreeSet<_> = view.members.iter().map(|m| &m.version_id).collect();
        let mut pending = Vec::new();
        for version in versions {
            let ready: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM vector_meta WHERE version_id=?1 AND model=?2 AND dims=?3 AND preprocessing_version=?4 AND generation_id=?5)",
                params![version, config.model, config.dims as i64, config.preprocessing_version, self.generation],
                |row| row.get(0),
            )?;
            if !ready {
                pending.push(Passage {
                    version_id: version.clone(),
                    text: self.payload(version)?.redacted_text,
                });
            }
        }
        Ok(pending)
    }

    /// Core supplies a view refreshed after model execution. Publish only its
    /// still-eligible versions, in the same transaction as queue removal.
    pub(crate) fn publish_embeddings(
        &self,
        config: &VectorConfig,
        current: &View,
        updates: Vec<VectorUpdate>,
    ) -> Result<VectorPublication> {
        let eligible: BTreeSet<_> = current.members.iter().map(|m| &m.version_id).collect();
        let mut result = VectorPublication {
            published: 0,
            discarded: 0,
            partial: 0,
        };
        let tx = self.db.unchecked_transaction()?;
        for update in updates {
            if !eligible.contains(&update.version_id) {
                result.discarded += 1;
                continue;
            }
            tx.execute(
                "INSERT OR REPLACE INTO vector_meta VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    update.version_id,
                    config.model,
                    config.dims as i64,
                    config.preprocessing_version,
                    self.generation,
                    if update.partial {
                        "partial_content"
                    } else {
                        "ready"
                    },
                    serde_json::to_string(&update.vector)?,
                ],
            )?;
            tx.execute(
                "DELETE FROM embed_queue WHERE version_id=?1",
                [&update.version_id],
            )?;
            result.published += 1;
            result.partial += usize::from(update.partial);
        }
        util::fault("before_vector_publish");
        tx.commit()?;
        Ok(result)
    }
}
