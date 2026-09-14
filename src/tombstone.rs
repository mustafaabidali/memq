use crate::error::{Error, Result};
use crate::records::Item;
use crate::repository::Repository;
use crate::store::{SCHEMA, Store};
use crate::util;
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub format: u32,
    pub identity_hash: String,
    pub scope: Predicate,
    pub forgotten_at: String,
    pub purge_epoch: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
}

impl Predicate {
    pub fn hash(&self) -> Result<String> {
        self.item_id
            .as_ref()
            .map(|s| Ok(util::hash(s)))
            .unwrap_or_else(|| util::hash_json(self))
    }

    pub fn matches(&self, id: &str, source: &str, recorded_at: Option<&str>) -> bool {
        if self.item_id.as_ref().is_some_and(|v| v != id) {
            return false;
        }
        if self
            .project_id
            .as_ref()
            .is_some_and(|v| !id.starts_with(&format!("mq:{v}:")))
        {
            return false;
        }
        if self.source_id.as_ref().is_some_and(|v| v != source) {
            return false;
        }
        if self.after.is_some() || self.before.is_some() {
            let Some(at) = recorded_at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            if self
                .after
                .as_ref()
                .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok_and(|d| at < d))
                || self
                    .before
                    .as_ref()
                    .is_some_and(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok_and(|d| at >= d))
            {
                return false;
            }
        }
        true
    }
}

impl Tombstone {
    pub fn validate(&self) -> Result<()> {
        if self.format != 1
            || self.identity_hash != self.scope.hash()?
            || chrono::DateTime::parse_from_rfc3339(&self.forgotten_at).is_err()
            || ulid::Ulid::from_string(&self.purge_epoch).is_err()
            || (self.scope.item_id.is_none() && self.scope.project_id.is_none())
            || self
                .scope
                .after
                .iter()
                .chain(self.scope.before.iter())
                .any(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
        {
            return Err(Error::new(
                "invalid_tombstone",
                "invalid tombstone predicate or identity",
            ));
        }
        Ok(())
    }
}

pub fn load(repo: &Repository, access: &Connection) -> Result<Vec<Tombstone>> {
    let mut known = BTreeMap::new();
    let mut stmt = access.prepare("SELECT json FROM tombstones")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let t: Tombstone = serde_json::from_str(&row?)
            .map_err(|_| Error::new("invalid_tombstone", "invalid local mirror"))?;
        t.validate()?;
        known.insert(t.identity_hash.clone(), t);
    }
    for path in repo.files_at(".memq/tombstones", None)? {
        if !path.ends_with(".json") {
            continue;
        }
        let bytes = repo
            .read_at(&path, None)?
            .ok_or_else(|| Error::new("invalid_tombstone", "tombstone disappeared during read"))?;
        let t: Tombstone = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new(
                "invalid_tombstone",
                json!({"path":path,"reason":"invalid content-free tombstone"}),
            )
        })?;
        t.validate()?;
        known.insert(t.identity_hash.clone(), t);
    }
    let tx = access.unchecked_transaction()?;
    for t in known.values() {
        tx.execute(
            "INSERT OR REPLACE INTO tombstones VALUES(?1,?2)",
            params![t.identity_hash, serde_json::to_string(t)?],
        )?;
    }
    tx.commit()?;
    Ok(known.into_values().collect())
}

pub fn excludes(tombstones: &[Tombstone], item: &Item) -> bool {
    tombstones.iter().any(|t| {
        t.scope.matches(
            &item.id,
            &item.source_id,
            item.observation.recorded_at.as_deref(),
        )
    })
}

pub fn excludes_id(tombstones: &[Tombstone], id: &str, recorded_at: Option<&str>) -> bool {
    let source = id.split(':').nth(2).unwrap_or("");
    tombstones
        .iter()
        .any(|t| t.scope.matches(id, source, recorded_at))
}

/// A remembered identity stays excluded even when mutable source timestamps
/// no longer match the predicate that originally forgot it.
pub fn excludes_record(
    tombstones: &[Tombstone],
    forgotten: &BTreeSet<String>,
    id: &str,
    source: &str,
    recorded_at: Option<&str>,
) -> bool {
    forgotten.contains(id)
        || tombstones
            .iter()
            .any(|t| t.scope.matches(id, source, recorded_at))
}

fn mark_purge_pending(access: &Connection) -> Result<()> {
    if access.execute("UPDATE purge_progress SET complete=0", [])? == 0 {
        // Persistent identities remain authoritative even if no predicate is
        // present. This content-free marker also survives a cache-cleanup crash.
        access.execute(
            "INSERT INTO purge_progress VALUES(?1,'identity',0)",
            [util::hash("memq:persistent-identity-purge")],
        )?;
    }
    Ok(())
}

fn remember_matching_ids(
    db: &Connection,
    access: &Connection,
    tombstones: &[Tombstone],
    forgotten: &mut BTreeSet<String>,
) -> Result<()> {
    let schema: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if schema > SCHEMA {
        return Err(Error::new(
            "unsupported_schema",
            "cannot purge a newer generation",
        ));
    }
    if schema == 0 || !crate::store::purge_layout(db) {
        return Err(Error::new(
            "purge_failed",
            "retained generation cannot be safely inspected for forgotten evidence",
        ));
    }
    let mut ids: BTreeSet<String> =
        crate::notes::purge_operations(db, access, tombstones, forgotten)?
            .into_iter()
            .collect();
    let mut stmt = db.prepare(
        "SELECT i.item_id,i.source_id,v.payload_json,o.json FROM items i
         LEFT JOIN item_versions v ON v.item_id=i.item_id
         LEFT JOIN observations o ON o.version_id=v.version_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (id, source, payload, obs) = row?;
        if forgotten.contains(&id) {
            ids.insert(id);
            continue;
        }
        let payload: Value = payload
            .as_ref()
            .map(|s| serde_json::from_str(s))
            .transpose()?
            .unwrap_or(Value::Null);
        let observation: Value = obs
            .as_ref()
            .map(|s| serde_json::from_str(s))
            .transpose()?
            .unwrap_or(Value::Null);
        let recorded = payload
            .pointer("/record/recorded_at")
            .and_then(Value::as_str)
            .or_else(|| observation["recorded_at"].as_str());
        if tombstones
            .iter()
            .any(|t| t.scope.matches(&id, &source, recorded))
        {
            ids.insert(id);
        }
    }
    drop(stmt);
    if !ids.is_empty() {
        // Commit identity before deleting any versions. An edited timestamp
        // must not release an identity during recovery of an interrupted purge.
        let tx = access.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO forgotten_items SELECT value FROM json_each(?1)",
            [serde_json::to_string(&ids)?],
        )?;
        mark_purge_pending(&tx)?;
        tx.commit()?;
        forgotten.extend(ids);
    }
    Ok(())
}

fn purge_db(
    db: &Connection,
    access: &Connection,
    tombstones: &[Tombstone],
    forgotten: &BTreeSet<String>,
    forgotten_json: &str,
) -> Result<()> {
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;
    // Restore missing derived FTS before deleting through its triggers.
    for (table, column, tokenizer) in [
        ("items_fts", "normalized_text", "unicode61"),
        ("items_trigram", "redacted_text", "trigram"),
    ] {
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |r| r.get(0),
        )?;
        if !present {
            db.execute_batch(&format!(
                "CREATE VIRTUAL TABLE {table} USING fts5({column},content=item_versions,content_rowid=vrow,tokenize='{tokenizer}',detail=full);
                 INSERT INTO {table}({table},rank) VALUES('secure-delete',1);
                 INSERT INTO {table}({table}) VALUES('rebuild');"
            ))?;
        }
    }
    // Discovery in another generation (or an edited authored note) can forget
    // an ID whose saved operation timestamp is outside the original interval.
    // Reserve its retry identity before purging that operation's original body.
    crate::notes::purge_operations(db, access, tombstones, forgotten)?;
    let tx = db.unchecked_transaction()?;
    let mut removed = 0;
    for sql in [
        "DELETE FROM view_items WHERE item_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM items WHERE item_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM item_versions WHERE item_id IN (SELECT value FROM json_each(?1))",
        "DELETE FROM relations WHERE to_item_id IN (SELECT value FROM json_each(?1))",
    ] {
        removed += tx.execute(sql, [forgotten_json])?;
    }
    if removed > 0 {
        // The marker commits before the index transaction. It must survive
        // even when a restart finds no rows left to identify cache cleanup.
        mark_purge_pending(access)?;
    }
    let pending: bool = access.query_row(
        "SELECT EXISTS(SELECT 1 FROM purge_progress WHERE complete=0)",
        [],
        |r| r.get(0),
    )?;
    if pending {
        tx.execute("DELETE FROM result_sets", [])?;
    }
    util::fault("mid_purge");
    tx.commit()?;
    if pending {
        // Rebuild also removes stale entries left by a missing delete trigger
        // in a damaged archive. VACUUM then removes their old SQLite pages.
        db.execute_batch(
            "INSERT INTO items_fts(items_fts) VALUES('rebuild');
             INSERT INTO items_trigram(items_trigram) VALUES('rebuild');",
        )?;
        db.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
    }
    Ok(())
}

pub fn apply(store: &Store, tombstones: &[Tombstone]) -> Result<()> {
    let mut forgotten = store.forgotten_ids()?;
    if tombstones.is_empty() && forgotten.is_empty() {
        return Ok(());
    }
    for t in tombstones {
        store.access.execute(
            "INSERT INTO purge_progress VALUES(?1,?2,0)
             ON CONFLICT(identity_hash) DO UPDATE SET epoch=excluded.epoch,complete=0
             WHERE purge_progress.epoch != excluded.epoch",
            params![t.identity_hash, t.purge_epoch],
        )?;
    }
    // Collect the complete identity set before deleting from any generation:
    // an old in-range version can identify a newer, out-of-range active version.
    remember_matching_ids(&store.db, &store.access, tombstones, &mut forgotten)?;
    let mut archives = Vec::new();
    let selected = fs::read_to_string(store.root.join("CURRENT")).unwrap_or_default();
    for entry in fs::read_dir(store.root.join("generations"))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy() == store.generation {
            continue;
        }
        let path = entry.path().join("index.sqlite");
        if !path.exists() {
            continue;
        }
        let db = Connection::open(&path)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != selected.trim()
            && ulid::Ulid::from_string(&name).is_ok()
            && entry.file_type()?.is_dir()
            && !entry.path().join("VERIFIED").exists()
            && crate::store::empty_build_database(&db).unwrap_or(false)
        {
            let mut empty = true;
            for file in fs::read_dir(entry.path())? {
                let file = file?;
                empty &= file.file_type()?.is_file()
                    && matches!(
                        file.file_name().to_str(),
                        Some("index.sqlite" | "index.sqlite-wal" | "index.sqlite-shm")
                    );
            }
            if empty {
                drop(db);
                fs::remove_dir_all(entry.path())?;
                util::sync_dir(&store.root.join("generations"))?;
                continue;
            }
        }
        match remember_matching_ids(&db, &store.access, tombstones, &mut forgotten) {
            Ok(()) => {
                drop(db);
                archives.push(path);
            }
            Err(e)
                if e.code == "storage_error"
                    && e.detail["message"].as_str().is_some_and(|s| {
                        s.contains("not a database") || s.contains("malformed")
                    }) =>
            {
                drop(db);
                // Corrupt derived state cannot be selectively purged. Its sources remain untouched.
                mark_purge_pending(&store.access)?;
                fs::remove_dir_all(entry.path())?;
                util::sync_dir(&store.root.join("generations"))?;
            }
            Err(e) => return Err(e),
        }
    }
    let forgotten_json = serde_json::to_string(&forgotten)?;
    purge_db(
        &store.db,
        &store.access,
        tombstones,
        &forgotten,
        &forgotten_json,
    )?;
    for path in archives {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        purge_db(&db, &store.access, tombstones, &forgotten, &forgotten_json)?;
    }
    store.access.execute(
        "DELETE FROM frecency WHERE item_id IN (SELECT value FROM json_each(?1))",
        [&forgotten_json],
    )?;
    let pending: bool = store.access.query_row(
        "SELECT EXISTS(SELECT 1 FROM purge_progress WHERE complete=0)",
        [],
        |r| r.get(0),
    )?;
    if pending {
        util::fault("before_vector_purge");
        let mut directories = BTreeSet::new();
        for entry in walkdir::WalkDir::new(store.root.join("generations")) {
            let entry =
                entry.map_err(|_| Error::new("purge_failed", "cannot inspect generations"))?;
            let vector_temp = entry.path().extension().is_some_and(|s| s == "tmp")
                && entry
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|stem| {
                        stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit())
                    });
            if entry.file_type().is_file()
                && (entry.path().extension().is_some_and(|s| s == "usearch") || vector_temp)
            {
                fs::remove_file(entry.path())?;
                directories.insert(entry.path().parent().expect("generation").to_owned());
            }
        }
        for directory in directories {
            util::sync_dir(&directory)?;
        }
    }
    store
        .access
        .execute("UPDATE purge_progress SET complete=1", [])?;
    store
        .access
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}
