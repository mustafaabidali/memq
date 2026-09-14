//! Core ordering for optional embeddings: refresh, run the model, refresh again,
//! then publish still-eligible versions through storage.
use crate::core::{ReadRequest, Service};
use crate::error::{Error, Result};
use crate::store::VectorUpdate;
use crate::{tombstone, vectors};
use serde_json::{Value, json};

impl Service {
    pub fn embed_pending(&mut self) -> Result<Value> {
        let config = self.config.vectors.clone().ok_or_else(|| {
            Error::new(
                "invalid_config",
                "configure an optional embedding command first",
            )
        })?;
        let mut view = self.reconcile(&ReadRequest::default(), "embed")?;
        let forgotten = self.store.forgotten_ids()?;
        view.members.retain(|member| {
            !tombstone::excludes_record(
                &self.tombstones,
                &forgotten,
                &member.id,
                &member.source_id,
                member.observation.recorded_at.as_deref(),
            )
        });
        let pending = self.store.pending_embeddings(&view, &config)?;
        let mut published = 0;
        let mut discarded = 0;
        let mut partial = 0;
        // Provisional process batch bound; it does not limit retained evidence.
        for batch in pending.chunks(32) {
            let response = vectors::embed(
                &config,
                &batch
                    .iter()
                    .map(|passage| passage.text.clone())
                    .collect::<Vec<_>>(),
                "passage",
            )?;
            let mut current = self.reconcile(&ReadRequest::default(), "embed")?;
            let forgotten = self.store.forgotten_ids()?;
            current.members.retain(|member| {
                !tombstone::excludes_record(
                    &self.tombstones,
                    &forgotten,
                    &member.id,
                    &member.source_id,
                    member.observation.recorded_at.as_deref(),
                )
            });
            let updates = batch
                .iter()
                .zip(response.vectors.into_iter().zip(response.truncated))
                .map(|(passage, (vector, partial))| VectorUpdate {
                    version_id: passage.version_id.clone(),
                    vector,
                    partial,
                })
                .collect();
            let result = self.store.publish_embeddings(&config, &current, updates)?;
            published += result.published;
            discarded += result.discarded;
            partial += result.partial;
        }
        Ok(json!({
            "model":config.model,"dimensions":config.dims,"preprocessing_version":config.preprocessing_version,
            "published":published,"discarded_changed_versions":discarded,"partial_content":partial,"parameters":"provisional"
        }))
    }
}
