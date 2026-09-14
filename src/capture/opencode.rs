//! OpenCode SQLite schema and complete scoped inventory scans.
use super::{Captured, ScopeCache, make_item, session, validate_metadata};
use crate::error::{Error, Result};
use crate::redact;
use crate::repository::Repository;
use crate::tombstone::{self, Tombstone};
use crate::util;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

pub fn schema(db: &Connection) -> Result<Value> {
    let required = [
        (
            "session",
            vec!["id", "directory", "version", "time_updated"],
        ),
        ("message", vec!["id", "session_id", "time_updated", "data"]),
        (
            "part",
            vec![
                "id",
                "session_id",
                "message_id",
                "time_created",
                "time_updated",
                "data",
            ],
        ),
    ];
    let mut tables = BTreeMap::new();
    for (table, columns) in required {
        let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
        let found = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if columns.iter().any(|c| !found.iter().any(|f| f == c)) {
            return Err(Error::new(
                "capture_schema",
                json!({"table":table,"required_columns":columns,"found_columns":found}),
            ));
        }
        tables.insert(table, found);
    }
    Ok(json!({"adapter":1,"tables":tables}))
}

pub(super) fn collect(
    repo: &Repository,
    project: &str,
    path: &Path,
    tombstones: &[Tombstone],
    scopes: &mut ScopeCache,
    out: &mut Captured,
) -> Result<Vec<Value>> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(2))?;
    schema(&db)?;
    db.execute_batch("BEGIN")?;
    let source = "harness-opencode";
    let relative = path
        .file_name()
        .ok_or_else(|| Error::new("capture_schema", "invalid database path"))?
        .to_string_lossy();
    let mut sessions =
        db.prepare("SELECT id,directory,version,time_updated FROM session ORDER BY id")?;
    let rows = sessions.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    let mut inventory = Vec::new();
    let mut watermark = 0i64;
    let mut seen = Vec::new();
    let mut errors = Vec::new();
    for row in rows {
        let (id, directory, version, updated) = row?;
        let session = match session(
            repo,
            &json!({"id":id,"cwd":directory}),
            scopes,
            &mut out.work,
        ) {
            Ok(Some(s)) => s,
            Ok(None) => continue,
            Err(e) => {
                inventory.push(json!(["rejected_session", util::hash(&id), e.code]));
                errors.push(json!({"reason":e.code,"detail":e.detail}));
                continue;
            }
        };
        // Native timestamps do not witness directory edits or changes in Git
        // scope at the same cwd. Bind validated metadata and fresh resolution.
        inventory.push(json!(["session_scope", util::hash_json(&session)?]));
        if !version.starts_with("1.18.") {
            // Matching table columns do not establish compatible semantics.
            // Pause this session, retaining its version in the input witness,
            // while supported sessions later in the inventory remain usable.
            inventory.push(json!([id, updated, util::hash(&version), "paused:version"]));
            errors.push(json!({
                "reason":"capture_unsupported_version","status":"paused",
                "session_id":id,"version":redact::text(&version),"supported":"1.18.x"
            }));
            continue;
        }
        inventory.push(json!([id, updated, util::hash(&version)]));
        watermark = watermark.max(updated);
        let mut stmt = db.prepare(
            "SELECT p.id,p.message_id,p.time_updated,p.time_created,p.data,m.data,m.time_updated
             FROM part p LEFT JOIN message m ON m.id=p.message_id AND m.session_id=p.session_id
             WHERE p.session_id=?1 ORDER BY p.id",
        )?;
        let parts = stmt.query_map([&id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        for part in parts {
            let (part_id, message_id, updated, created, data, message, message_updated) = part?;
            if let Err(e) = validate_metadata(&part_id, "entry_id")
                .and_then(|()| validate_metadata(&message_id, "message_id"))
            {
                inventory.push(json!(["rejected_part", id, util::hash(&part_id), e.code]));
                errors.push(json!({"reason":e.code,"detail":e.detail}));
                continue;
            }
            let message = message.ok_or_else(|| {
                Error::new("capture_schema", "part has no matching session message")
            })?;
            let payload: Value = serde_json::from_str(&data)
                .map_err(|_| Error::new("capture_schema", "invalid part JSON"))?;
            let message: Value = serde_json::from_str(&message)
                .map_err(|_| Error::new("capture_schema", "invalid message JSON"))?;
            if !payload["type"].is_string() || !message["role"].is_string() {
                return Err(Error::new(
                    "capture_schema",
                    "part type or message role missing",
                ));
            }
            inventory.push(json!([
                id,
                part_id,
                message_id,
                created,
                updated,
                util::hash(data.as_bytes()),
                util::hash_json(&message)?
            ]));
            watermark = watermark.max(updated).max(message_updated.unwrap_or(0));
            seen.push(format!("{id}/{part_id}"));
            let timestamp =
                chrono::DateTime::from_timestamp_millis(created).map(|d| d.to_rfc3339());
            let raw = json!({"part":payload,"message_id":message_id,"message":message,"timestamp":timestamp});
            let text = String::from_utf8(util::canonical(&redact::value(&raw))?).expect("JSON");
            let mut item = make_item(
                project,
                source,
                &session,
                &format!("part:{part_id}"),
                text,
                &relative,
                format!(
                    "opencode:{}#part:{}",
                    util::escape(&relative),
                    util::escape(&part_id)
                ),
                [0, 0],
                &raw,
                util::hash(data),
            )?;
            item.observation.source_root = path.parent().map(|p| p.to_string_lossy().into_owned());
            if !tombstone::excludes_record(
                tombstones,
                &out.forgotten_ids,
                &item.id,
                &item.source_id,
                item.observation.recorded_at.as_deref(),
            ) {
                out.items.push(item);
            }
        }
    }
    let fingerprint = util::hash_json(&inventory)?;
    out.inventory.push(json!([source, relative, fingerprint]));
    out.cursors.push((
        source.into(),
        relative.into_owned(),
        json!({
            "last_time_updated":watermark,"seen_ids_window":seen,"inventory_sha256":fingerprint,
            "strategy":"complete_scoped_scan","complete":errors.is_empty()
        }),
    ));
    db.execute_batch("COMMIT")?;
    Ok(errors)
}
