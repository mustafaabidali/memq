use crate::config::Config;
use crate::error::{Error, Result};
use crate::records::{validate_note, validate_verification};
use crate::redact;
use crate::repository::Repository;
use crate::store::Store;
use crate::tombstone::{self, Tombstone};
use crate::util;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoteRequest {
    pub text: String,
    pub idempotency_key: String,
    #[serde(default = "progress")]
    pub kind: String,
    pub task: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub verification: Option<Value>,
    pub harness: Option<String>,
    pub session: Option<String>,
}

fn progress() -> String {
    "progress".into()
}

pub fn semantic(note: &Value) -> Result<String> {
    util::hash_json(&json!({
        "kind":note["kind"],"text":note["text"],"scope":note["scope"],
        "evidence":note["evidence"],"verification":note["verification"]
    }))
}

fn operation_key(note: &Value) -> String {
    note["provenance"]["idempotency_key_sha256"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| {
            util::hash(
                note["provenance"]["idempotency_key"]
                    .as_str()
                    .expect("validated"),
            )
        })
}

fn legacy_redacted_key(note: &Value) -> bool {
    note["provenance"].get("idempotency_key_sha256").is_none()
        && note["provenance"]["idempotency_key"]
            .as_str()
            .is_some_and(|s| s.contains("[REDACTED]"))
}

fn all_note_files(repo: &Repository, config: &Config) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    for source in &config.source {
        if source.kind == "memq-notes" {
            for path in repo
                .files_at(&source.path, None)?
                .into_iter()
                .filter(|p| p.ends_with(".json"))
            {
                files.push((source.id.clone(), path));
            }
        }
    }
    Ok(files)
}

fn remember_forgotten(
    access: &Connection,
    project: &str,
    key: &str,
    id: &str,
    semantic: &str,
    legacy_key: bool,
) -> Result<()> {
    let tx = access.unchecked_transaction()?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT item_id,semantic_sha256 FROM forgotten_note_ops WHERE project_id=?1 AND key=?2",
            params![project, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if existing.is_some_and(|(old_id, old_semantic)| old_id != id || old_semantic != semantic) {
        return Err(Error::new(
            "idempotency_conflict",
            "authored note disagrees with the forgotten operation",
        ));
    }
    tx.execute(
        "INSERT OR IGNORE INTO forgotten_note_ops VALUES(?1,?2,?3,?4,?5)",
        params![project, key, id, semantic, legacy_key],
    )?;
    tx.execute("INSERT OR IGNORE INTO forgotten_items VALUES(?1)", [id])?;
    tx.commit()?;
    Ok(())
}

// Old reservations did not carry source identity. Recover it from the saved
// source configuration or an explicit identity, never from the default source
// name. New reservations persist item_id before writing any note bytes.
fn legacy_operation_id(
    db: &Connection,
    project: &str,
    native: &str,
    worktree: &str,
    path: &str,
    tombstones: &[Tombstone],
) -> Result<String> {
    let prefix = format!("mq:{project}:");
    let mut ids = BTreeSet::new();
    let mut stmt = db.prepare("SELECT item_id FROM items WHERE native_id=?1")?;
    for row in stmt.query_map([native], |r| r.get::<_, String>(0))? {
        let id = row?;
        if id.starts_with(&prefix) {
            ids.insert(id);
        }
    }
    let mut stmt = db.prepare("SELECT source_id,config_json FROM sources WHERE worktree_key=?1")?;
    for row in stmt.query_map([worktree], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (source, config) = row?;
        let config: Value = serde_json::from_str(&config)?;
        if config["kind"] == "memq-notes"
            && config["path"]
                .as_str()
                .is_some_and(|root| path.starts_with(&format!("{}/", root.trim_end_matches('/'))))
        {
            ids.insert(util::item_id(project, &source, native));
        }
    }
    for tombstone in tombstones {
        if let Some(id) = &tombstone.scope.item_id
            && id.starts_with(&prefix)
            && id.ends_with(&format!(":{native}"))
        {
            ids.insert(id.clone());
        }
    }
    if ids.len() == 1 {
        return Ok(ids.into_iter().next().expect("one identity"));
    }
    Err(Error::new(
        "note_recovery_failed",
        "cannot establish the original source of a legacy note operation",
    ))
}

fn remove_operation_temp(db: &Connection, worktree: &str, path: &str, native: &str) -> Result<()> {
    util::relative_path(path)?;
    if ulid::Ulid::from_string(native).is_err()
        || Path::new(path).file_name().and_then(|s| s.to_str()) != Some(&format!("{native}.json"))
    {
        return Err(Error::new(
            "note_recovery_failed",
            "invalid note operation path",
        ));
    }
    let temp = Path::new(worktree)
        .join(path)
        .with_file_name(format!(".{native}.tmp"));
    match fs::symlink_metadata(&temp) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(Error::new(
                "purge_failed",
                "note temporary path is not a regular file",
            ));
        }
        Ok(_) => (),
    }
    let repo = Repository::discover(Path::new(worktree))?;
    let clone: String = db.query_row("SELECT value FROM meta WHERE key='clone_id'", [], |r| {
        r.get(0)
    })?;
    let parent = temp.parent().expect("temporary note directory");
    if repo.clone_id != clone || !fs::canonicalize(parent)?.starts_with(&repo.root) {
        return Err(Error::new(
            "purge_failed",
            "note temporary path is outside its recorded clone",
        ));
    }
    fs::remove_file(&temp)?;
    util::sync_dir(parent)?;
    Ok(())
}

/// Purge operations independently of derived item membership. Commit the
/// content-free retry identity first so a crash can never release a used key.
pub(crate) fn purge_operations(
    db: &Connection,
    access: &Connection,
    tombstones: &[Tombstone],
    known_forgotten: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let present: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='note_ops')",
        [],
        |r| r.get(0),
    )?;
    if !present {
        // A damaged archived index may have lost this reconstructible table.
        // There are no operation bytes left in it to purge.
        return Ok(Vec::new());
    }
    let has_identity = db.prepare("SELECT item_id FROM note_ops LIMIT 0").is_ok();
    let mut stmt = db.prepare(&format!(
        "SELECT project_id,key,note_id,semantic_sha256,worktree,path,note_json,{}
         FROM note_ops WHERE op='note'",
        if has_identity { "item_id" } else { "NULL" },
    ))?;
    let mut forgotten = Vec::new();
    for row in stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, Option<String>>(7)?,
        ))
    })? {
        let (project, key, native, semantic, worktree, path, body, id) = row?;
        let note: Value = serde_json::from_str(&body)?;
        validate_note(&note)?;
        let id = match id {
            Some(id) => id,
            None => legacy_operation_id(db, &project, &native, &worktree, &path, tombstones)?,
        };
        if !tombstone::excludes_record(
            tombstones,
            known_forgotten,
            &id,
            id.split(':').nth(2).unwrap_or(""),
            note["recorded_at"].as_str(),
        ) {
            continue;
        }
        remember_forgotten(
            access,
            &project,
            &key,
            &id,
            &semantic,
            legacy_redacted_key(&note),
        )?;
        remove_operation_temp(db, &worktree, &path, &native)?;
        forgotten.push((project, key, id));
    }
    drop(stmt);
    if !forgotten.is_empty() {
        access.execute("UPDATE purge_progress SET complete=0", [])?;
    }
    let tx = db.unchecked_transaction()?;
    for (project, key, _) in &forgotten {
        tx.execute(
            "DELETE FROM note_ops WHERE project_id=?1 AND op='note' AND key=?2",
            params![project, key],
        )?;
    }
    tx.commit()?;
    Ok(forgotten.into_iter().map(|(_, _, id)| id).collect())
}

struct AuthoredNote {
    id: String,
    native: String,
    semantic: String,
    worktree: String,
    path: String,
    note: Value,
    legacy_key: bool,
}

/// Returns conflicting note IDs grouped by hashed operation key. A conflict
/// leaves that key's reservation unchanged; unrelated keys can still progress.
pub fn backfill(
    repo: &Repository,
    config: &Config,
    store: &Store,
    tombstones: &[Tombstone],
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut notes = BTreeMap::<String, AuthoredNote>::new();
    let mut conflicts = BTreeMap::<String, BTreeSet<String>>::new();
    let mut files = Vec::new();
    for worktree in repo.worktrees()? {
        // A linked worktree may legitimately carry a different configuration.
        // Only available sources declaring this project participate in retries.
        let Ok(local_config) = Config::load(&worktree.root) else {
            continue;
        };
        if local_config.project_id == config.project_id {
            for (source, path) in all_note_files(&worktree, &local_config)? {
                files.push((worktree.clone(), source, path));
            }
        }
    }
    for (repo, source, path) in files {
        let bytes = repo
            .read_at(&path, None)?
            .ok_or_else(|| Error::new("invalid_note", "note disappeared"))?;
        let note: Value = serde_json::from_slice(&bytes)?;
        validate_note(&note)?;
        if note["scope"]["project_id"] != config.project_id {
            continue;
        }
        let id = util::item_id(
            &config.project_id,
            &source,
            note["id"].as_str().expect("validated"),
        );
        // Identity is computed before redaction. Different opaque keys can
        // have the same redacted display text.
        let key = operation_key(&note);
        let legacy_lost_key = legacy_redacted_key(&note);
        let mut note = redact::value(&note);
        if !legacy_lost_key {
            note["provenance"]["idempotency_key_sha256"] = json!(key);
        }
        let native = note["id"].as_str().expect("validated").to_owned();
        let sem = semantic(&note)?;
        if let Some(other) = notes.get(&key) {
            if other.id != id || other.semantic != sem {
                conflicts
                    .entry(key)
                    .or_default()
                    .extend([other.native.clone(), native]);
            }
            // The same authored note can be checked out in several worktrees.
            continue;
        }
        notes.insert(
            key,
            AuthoredNote {
                id,
                native,
                semantic: sem,
                worktree: repo.root.to_string_lossy().into_owned(),
                path,
                note,
                legacy_key: legacy_lost_key,
            },
        );
    }
    for (key, authored) in notes {
        let existing: Option<(String, String, Option<String>)> = store.db.query_row(
            "SELECT note_id,semantic_sha256,item_id FROM note_ops WHERE project_id=?1 AND op='note' AND key=?2",
            params![config.project_id,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))
        ).optional()?;
        if let Some((native, hash, id)) = existing
            && (native != authored.native
                || hash != authored.semantic
                || id.is_some_and(|id| id != authored.id))
        {
            conflicts
                .entry(key.clone())
                .or_default()
                .extend([native, authored.native.clone()]);
        }
        let forgotten: Option<(String, String)> = store.access.query_row(
            "SELECT item_id,semantic_sha256 FROM forgotten_note_ops WHERE project_id=?1 AND key=?2",
            params![config.project_id,key],|r|Ok((r.get(0)?,r.get(1)?))
        ).optional()?;
        if let Some((id, hash)) = &forgotten
            && (*id != authored.id || *hash != authored.semantic)
        {
            conflicts.entry(key.clone()).or_default().extend([
                id.rsplit(':').next().expect("note identity").to_owned(),
                authored.native.clone(),
            ]);
        }
        if conflicts.contains_key(&key) {
            continue;
        }
        if tombstone::excludes_id(
            tombstones,
            &authored.id,
            authored.note["recorded_at"].as_str(),
        ) || store.is_forgotten(&authored.id)?
            || forgotten.is_some()
        {
            remember_forgotten(
                &store.access,
                &config.project_id,
                &key,
                &authored.id,
                &authored.semantic,
                authored.legacy_key,
            )?;
            continue;
        }
        store.db.execute(
            "INSERT INTO note_ops(project_id,op,key,note_id,semantic_sha256,state,worktree,path,note_json,item_id)
             VALUES(?1,'note',?2,?3,?4,'durable',?5,?6,?7,?8)
             ON CONFLICT(project_id,op,key) DO UPDATE SET state='durable',item_id=excluded.item_id",
            params![
                config.project_id,
                key,
                authored.native,
                authored.semantic,
                authored.worktree,
                authored.path,
                serde_json::to_string(&authored.note)?,
                authored.id,
            ],
        )?;
    }
    Ok(conflicts)
}

pub fn write(
    repo: &Repository,
    config: &Config,
    store: &Store,
    tombstones: &[Tombstone],
    request: NoteRequest,
) -> Result<Value> {
    if !matches!(request.kind.as_str(), "progress" | "note" | "verification")
        || request.idempotency_key.is_empty()
        || request.idempotency_key.len() > 512
        || request.text.trim().is_empty()
    {
        return Err(Error::new(
            "invalid_note",
            "kind, nonempty text and idempotency_key are required",
        ));
    }
    if request.kind == "verification" {
        validate_verification(request.verification.as_ref().unwrap_or(&Value::Null))?;
    } else if request.verification.is_some() {
        return Err(Error::new(
            "invalid_note",
            "verification requires kind=verification",
        ));
    }
    let source = config
        .source
        .iter()
        .find(|s| s.kind == "memq-notes")
        .ok_or_else(|| Error::new("invalid_note", "configure a memq-notes source"))?;
    let conflicts = backfill(repo, config, store, tombstones)?;
    let key = util::hash(&request.idempotency_key);
    if let Some(note_ids) = conflicts.get(&key) {
        return Err(Error::new(
            "idempotency_conflict",
            json!({"reason":"conflicting_authored_notes","note_ids":note_ids}),
        ));
    }
    let display_key = redact::text(&request.idempotency_key);
    if display_key.contains("[REDACTED]") {
        let forgotten_legacy: bool = store.access.query_row(
            "SELECT EXISTS(SELECT 1 FROM forgotten_note_ops WHERE project_id=?1 AND key=?2 AND legacy_key=1)",
            params![config.project_id, util::hash(&display_key)],
            |r| r.get(0),
        )?;
        let legacy: Option<String> = store
            .db
            .query_row(
                "SELECT note_json FROM note_ops WHERE project_id=?1 AND op='note' AND key=?2",
                params![config.project_id, util::hash(&display_key)],
                |r| r.get(0),
            )
            .optional()?;
        if forgotten_legacy
            || conflicts.contains_key(&util::hash(&display_key))
            || legacy
                .map(|stored| {
                    serde_json::from_str::<Value>(&stored).map(|v| legacy_redacted_key(&v))
                })
                .transpose()?
                .unwrap_or(false)
        {
            // An early prototype persisted only the redacted key. Its original
            // identity cannot be recovered safely; keep the authored note.
            return Err(Error::new(
                "legacy_idempotency_key_unrecoverable",
                "an older note lost its retry key during redaction; preserve it and use a new key without a redaction marker",
            ));
        }
    }
    let state = repo.state()?;
    let mut note = redact::value(&json!({
        "format":1,"id":util::id(),"kind":request.kind,"text":request.text,"recorded_at":util::now(),
        "scope":{"project_id":config.project_id,"branch":state.branch,"task":request.task},
        "provenance":{"harness":request.harness,"session":request.session,
            "idempotency_key":request.idempotency_key,"idempotency_key_sha256":key},
        "evidence":request.evidence,"verification":request.verification,
        "observed":{"head":state.head,"object_format":state.object_format,"dirty":!state.dirty.is_empty()}
    }));
    let sem = semantic(&note)?;
    let forgotten: Option<(String, String)> = store
        .access
        .query_row(
            "SELECT item_id,semantic_sha256 FROM forgotten_note_ops WHERE project_id=?1 AND key=?2",
            params![config.project_id, key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((id, old_sem)) = forgotten {
        if sem != old_sem {
            return Err(Error::new(
                "idempotency_conflict",
                "idempotency key has a different semantic payload",
            ));
        }
        return Ok(json!({"id":id,"availability":"forgotten"}));
    }
    let existing: Option<(String,String,String,String,String,Option<String>)> = store.db.query_row(
        "SELECT note_id,semantic_sha256,worktree,path,note_json,item_id FROM note_ops WHERE project_id=?1 AND op='note' AND key=?2",
        params![config.project_id,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))
    ).optional()?;
    let reserve = existing.is_none();
    let (native, worktree, path, id) =
        if let Some((native, old_sem, worktree, path, stored, id)) = existing {
            if sem != old_sem {
                return Err(Error::new(
                    "idempotency_conflict",
                    "idempotency key has a different semantic payload",
                ));
            }
            note = serde_json::from_str(&stored)?;
            let id = match id {
                Some(id) => id,
                None => {
                    let id = legacy_operation_id(
                        &store.db,
                        &config.project_id,
                        &native,
                        &worktree,
                        &path,
                        tombstones,
                    )?;
                    // Pin the recovered identity before another file-write
                    // interruption; the current default source is not its origin.
                    store.db.execute(
                    "UPDATE note_ops SET item_id=?3 WHERE project_id=?1 AND op='note' AND key=?2",
                    params![config.project_id, key, id],
                )?;
                    id
                }
            };
            (native, worktree, path, id)
        } else {
            let native = note["id"].as_str().expect("id").to_owned();
            let id = util::item_id(&config.project_id, &source.id, &native);
            let path = format!("{}/{}.json", source.path, native);
            let worktree = repo.root.to_string_lossy().into_owned();
            (native, worktree, path, id)
        };
    if tombstone::excludes_id(tombstones, &id, note["recorded_at"].as_str()) {
        remember_forgotten(&store.access, &config.project_id, &key, &id, &sem, false)?;
        return Ok(json!({"id":id,"availability":"forgotten"}));
    }
    if reserve {
        store.db.execute(
            "INSERT INTO note_ops(project_id,op,key,note_id,semantic_sha256,state,worktree,path,note_json,item_id)
             VALUES(?1,'note',?2,?3,?4,'reserved',?5,?6,?7,?8)",
            params![
                config.project_id,
                key,
                native,
                sem,
                worktree,
                path,
                serde_json::to_string(&note)?,
                id,
            ],
        )?;
    }
    util::fault("after_note_reservation");
    let root = Path::new(&worktree);
    let recorded_repo = Repository::discover(root)?;
    if recorded_repo.common != repo.common {
        return Err(Error::new(
            "note_recovery_failed",
            "recorded worktree no longer belongs to this clone",
        ));
    }
    util::relative_path(&path)?;
    let final_path = root.join(&path);
    let parent = final_path.parent().expect("note directory");
    fs::create_dir_all(parent)?;
    if !fs::canonicalize(parent)?.starts_with(&recorded_repo.root) {
        return Err(Error::new(
            "invalid_note",
            "notes directory escapes recorded worktree",
        ));
    }
    let temp = parent.join(format!(".{native}.tmp"));
    if fs::symlink_metadata(&final_path).is_ok_and(|m| !m.file_type().is_file())
        || fs::symlink_metadata(&temp).is_ok_and(|m| !m.file_type().is_file())
    {
        return Err(Error::new(
            "invalid_note",
            "note paths must be regular files",
        ));
    }
    if final_path.exists() {
        let current: Value = serde_json::from_slice(&fs::read(&final_path)?)?;
        validate_note(&current)?;
        if current["id"] != native
            || semantic(&redact::value(&current))? != sem
            || operation_key(&current) != key
        {
            return Err(Error::new(
                "idempotency_conflict",
                "final note collision; authored file was preserved",
            ));
        }
        if temp.exists() {
            fs::remove_file(&temp)?;
        }
    } else {
        if temp.exists() {
            let valid = fs::read(&temp)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| v == note && validate_note(&v).is_ok());
            if !valid {
                fs::remove_file(&temp)?;
            }
        }
        if !temp.exists() {
            store.db.execute(
                "UPDATE note_ops SET state='writing' WHERE project_id=?1 AND op='note' AND key=?2",
                params![config.project_id, key],
            )?;
            util::write_private(&temp, &util::canonical(&note)?)?;
        }
        util::fault("after_note_tmp");
        util::publish_new(&temp, &final_path).map_err(|e| {
            if final_path.exists() {
                Error::new("idempotency_conflict", "note publication collision")
            } else {
                e
            }
        })?;
    }
    util::fault("after_rename_before_stage");
    store.db.execute(
        "UPDATE note_ops SET state='durable' WHERE project_id=?1 AND op='note' AND key=?2",
        params![config.project_id, key],
    )?;
    let staging = stage(&recorded_repo, &path)?;
    let mut result = json!({
        "id":id,"note_id":native,"durability":"durable","staging":staging["status"],
        "staging_detail":staging["detail"],"path":path,
        "committed":false,"claim": if request.kind=="verification" { json!("reported") } else { Value::Null }
    });
    if !conflicts.is_empty() {
        result["duplicates"] = json!(
            conflicts
                .values()
                .map(|note_ids| json!({"note_ids":note_ids}))
                .collect::<Vec<_>>()
        );
    }
    Ok(result)
}

pub fn stage(repo: &Repository, path: &str) -> Result<Value> {
    let ignored = repo.git(&["check-ignore", "--quiet", "--", path])?;
    match ignored.status.code() {
        Some(0) => Ok(json!({"status":"ignored","detail":null})),
        Some(1) => {
            let result = repo.git(&["add", "--", path])?;
            if result.status.success() {
                Ok(json!({"status":"staged","detail":null}))
            } else {
                Ok(
                    json!({"status":"failed","detail":{"operation":"git add","exit":result.status.code()}}),
                )
            }
        }
        other => {
            Ok(json!({"status":"failed","detail":{"operation":"git check-ignore","exit":other}}))
        }
    }
}
