//! Recoverable, content-free JSONL cache manifests and verified prefix reuse.
use super::{
    CaptureCacheInput, CaptureWork, ScopeCache, Session, codex, make_item, omp, session,
    validate_metadata, validate_path,
};
use crate::error::Result;
use crate::records::Item;
use crate::redact;
use crate::repository::Repository;
use crate::tombstone::{self, Tombstone};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, Metadata};
use std::path::Path;
use std::sync::LazyLock;

// A cache cannot outlive changes to parsing, identity construction, canonical
// payload serialization, scope validation, or redaction. These revisions do
// not depend on cache hits, process state, or source inventory.
static BUILD: LazyLock<String> = LazyLock::new(|| {
    util::hash(concat!(
        env!("CARGO_PKG_VERSION"),
        "\n",
        include_str!("../../Cargo.lock")
    ))
});
static PARSERS: LazyLock<[String; 2]> = LazyLock::new(|| {
    let shared = concat!(
        include_str!("../capture.rs"),
        include_str!("jsonl.rs"),
        include_str!("jsonl_cache.rs"),
        include_str!("../records.rs"),
        include_str!("../util.rs")
    );
    [
        util::hash(format!("{shared}\n{}\n{}", *BUILD, include_str!("omp.rs"))),
        util::hash(format!(
            "{shared}\n{}\n{}",
            *BUILD,
            include_str!("codex.rs")
        )),
    ]
});
static REDACTION: LazyLock<String> =
    LazyLock::new(|| util::hash(format!("{}\n{}", *BUILD, include_str!("../redact.rs"))));

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    project_id: String,
    clone_id: String,
    source_id: String,
    source_path: String,
    source_root: String,
    root_sha256: String,
    parser_revision: String,
    redaction_revision: String,
    pub inode: u64,
    pub device: u64,
}

impl Binding {
    pub fn new(
        repo: &Repository,
        project: &str,
        kind: &str,
        root: &Path,
        relative: &str,
        metadata: &Metadata,
    ) -> Result<Self> {
        #[cfg(unix)]
        let (inode, device) = {
            use std::os::unix::fs::MetadataExt;
            (metadata.ino(), metadata.dev())
        };
        #[cfg(not(unix))]
        let (inode, device) = (0, 0);
        Ok(Self {
            project_id: project.into(),
            clone_id: repo.clone_id.clone(),
            source_id: format!("harness-{kind}"),
            source_path: relative.into(),
            source_root: root.to_string_lossy().into_owned(),
            root_sha256: util::hash(fs::canonicalize(root)?.as_os_str().as_encoded_bytes()),
            parser_revision: PARSERS[usize::from(kind == "codex")].clone(),
            redaction_revision: REDACTION.clone(),
            inode,
            device,
        })
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Manifest {
    format: u32,
    pub binding: Binding,
    pub prefix_len: u64,
    pub prefix_sha256: String,
    pub session: Session,
    pub header_end: u64,
    pub header_ordinal: Option<u64>,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Entry {
    pub id: String,
    pub byte_range: [u64; 2],
    pub source_sha256: String,
    pub recorded_at: Option<String>,
    pub state: EntryState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum EntryState {
    Retained { version_id: String },
    CredentialPin,
    Tombstoned,
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl Manifest {
    pub fn new(
        binding: Binding,
        prefix_len: u64,
        prefix_sha256: String,
        session: Session,
        header_end: u64,
        header_ordinal: Option<u64>,
        entries: Vec<Entry>,
    ) -> Self {
        Self {
            format: 1,
            binding,
            prefix_len,
            prefix_sha256,
            session,
            header_end,
            header_ordinal,
            entries,
        }
    }

    pub fn decode(cursor: &Value) -> Option<Self> {
        if cursor["complete"] != true {
            return None;
        }
        let value = cursor.get("cache")?;
        if cursor["cache_sha256"].as_str()? != util::hash_json(value).ok()? {
            return None;
        }
        let manifest: Self = serde_json::from_value(value.clone()).ok()?;
        let binding = &manifest.binding;
        let codex = match binding.source_id.as_str() {
            "harness-codex" => true,
            "harness-omp" => false,
            _ => return None,
        };
        if manifest.format != 1
            || binding.parser_revision != PARSERS[usize::from(codex)]
            || binding.redaction_revision != *REDACTION
            || !digest(&manifest.prefix_sha256)
            || manifest.header_end == 0
            || manifest.header_end > manifest.prefix_len
            || cursor["byte_offset"] != manifest.prefix_len
            || cursor["size"] != manifest.prefix_len
            || cursor["inventory_sha256"] != manifest.prefix_sha256
            || cursor["fingerprint"]["inode"] != binding.inode
            || cursor["fingerprint"]["device"] != binding.device
            || (codex && manifest.header_ordinal.is_none())
        {
            return None;
        }
        let mut end = manifest.header_end;
        let mut ordinal = manifest.header_ordinal;
        let mut ids = BTreeSet::new();
        for entry in &manifest.entries {
            if entry.byte_range[0] != end
                || entry.byte_range[1] <= end
                || entry.byte_range[1] > manifest.prefix_len
                || !digest(&entry.source_sha256)
                || validate_metadata(&entry.id, "entry_id").is_err()
                || !ids.insert(&entry.id)
                || entry
                    .recorded_at
                    .as_ref()
                    .is_some_and(|value| redact::text(value) != *value)
            {
                return None;
            }
            match &entry.state {
                EntryState::Retained { version_id } if !digest(version_id) => return None,
                EntryState::CredentialPin if codex || entry.recorded_at.is_some() => return None,
                _ => (),
            }
            if codex {
                let value = entry.id.parse::<u64>().ok()?;
                if value.to_string() != entry.id || ordinal.is_some_and(|before| value <= before) {
                    return None;
                }
                ordinal = Some(value);
            }
            end = entry.byte_range[1];
        }
        let last = manifest.entries.last().map(|entry| entry.id.as_str());
        if end != manifest.prefix_len || cursor["last_id_or_ordinal"] != json!(last) {
            return None;
        }
        Some(manifest)
    }

    pub fn write(&self, cursor: &mut Value) -> Result<()> {
        let value = serde_json::to_value(self)?;
        cursor["cache_sha256"] = json!(util::hash_json(&value)?);
        cursor["cache"] = value;
        Ok(())
    }

    pub fn referenced_versions(&self) -> impl Iterator<Item = String> + '_ {
        self.entries.iter().filter_map(|entry| match &entry.state {
            EntryState::Retained { version_id } => Some(version_id.clone()),
            _ => None,
        })
    }

    /// Called only after verifying every byte of the saved prefix. Stage all
    /// records locally: one missing/corrupt payload invalidates this whole cache.
    pub fn restore(
        &mut self,
        repo: &Repository,
        input: &CaptureCacheInput,
        tombstones: &[Tombstone],
        scopes: &mut ScopeCache,
        work: &mut CaptureWork,
    ) -> Option<Vec<Item>> {
        let b = &self.binding;
        validate_path(Path::new(&b.source_root), "source_path").ok()?;
        validate_path(Path::new(&b.source_path), "source_path").ok()?;
        let refreshed = session(
            repo,
            &json!({"id":self.session.id,"cwd":self.session.cwd,
                "branch":self.session.branch,"git":self.session.native_revision}),
            scopes,
            work,
        )
        .ok()??;
        if refreshed != self.session {
            return None;
        }
        let mut items = Vec::new();
        for entry in &mut self.entries {
            if matches!(entry.state, EntryState::CredentialPin) {
                continue;
            }
            let native = format!("{}/{}", self.session.id, entry.id);
            let id = util::item_id(&b.project_id, &b.source_id, &native);
            // Do this before looking up a body: a legitimate purge removes
            // payloads. A new candidate must not reference forgotten versions.
            if tombstone::excludes_record(
                tombstones,
                &input.forgotten_ids,
                &id,
                &b.source_id,
                entry.recorded_at.as_deref(),
            ) {
                entry.state = EntryState::Tombstoned;
                continue;
            }
            let EntryState::Retained { version_id } = &entry.state else {
                return None;
            };
            let payload = input.payloads_by_version.get(version_id)?;
            if payload.native_id != native
                || payload.kind != b.source_id
                || payload.native_status.is_some()
                || !payload.record.is_null()
                || !payload.sections.is_empty()
            {
                return None;
            }
            let pointer = if b.source_id == "harness-codex" {
                codex::pointer(&b.source_path, &entry.id)
            } else {
                omp::pointer(&b.source_path, &entry.id)
            };
            let mut item = make_item(
                &b.project_id,
                &b.source_id,
                &self.session,
                &entry.id,
                payload.redacted_text.clone(),
                &b.source_path,
                pointer,
                entry.byte_range,
                &json!({"timestamp":entry.recorded_at}),
                entry.source_sha256.clone(),
            )
            .ok()?;
            if item.version_id != *version_id {
                return None;
            }
            item.observation.source_root = Some(b.source_root.clone());
            items.push(item);
        }
        Some(items)
    }
}
