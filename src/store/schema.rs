//! Store layout, compatibility checks, and migrations. Generation
//! selection, recovery, locking, and atomic publication remain in the parent.
use crate::error::{Error, Result};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

pub const SCHEMA: i64 = 3;

pub(super) fn schema_version(db: &Connection) -> Result<i64> {
    Ok(db.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub(super) fn compatible(db: &Connection) -> Result<()> {
    if schema_version(db)? > SCHEMA {
        return Err(Error::new(
            "unsupported_schema",
            "this binary cannot read a newer store; no repair or downgrade attempted",
        ));
    }
    Ok(())
}

pub(super) fn has_table(db: &Connection, table: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}

pub(crate) fn empty_build_database(db: &Connection) -> Result<bool> {
    Ok(schema_version(db)? == 0
        && db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? == "ok"
        && db.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })? == 0
        && db.query_row("PRAGMA page_count", [], |r| r.get::<_, i64>(0))? <= 1
        && db.query_row("PRAGMA freelist_count", [], |r| r.get::<_, i64>(0))? == 0)
}

pub(crate) fn purge_layout(db: &Connection) -> bool {
    history_layout(db) && [
        "SELECT item_id,source_id,native_id FROM items LIMIT 0",
        "SELECT vrow,item_id,version_id,payload_json,redacted_text,normalized_text FROM item_versions LIMIT 0",
        "SELECT version_id,json FROM observations LIMIT 0",
        "SELECT to_item_id FROM relations LIMIT 0",
        "SELECT key FROM result_sets LIMIT 0",
    ].iter().all(|sql| db.prepare(sql).is_ok())
}

pub(super) fn history_layout(db: &Connection) -> bool {
    [
        "SELECT key,value FROM meta LIMIT 0",
        "SELECT view_id,scope_hash,inputs_hash,meta_json,is_current FROM views LIMIT 0",
        "SELECT view_id,member_seq,item_id,version_id,member_json FROM view_items LIMIT 0",
        "SELECT version_id,payload_json,redacted_text FROM item_versions LIMIT 0",
    ]
    .iter()
    .all(|sql| db.prepare(sql).is_ok())
}

pub(super) fn index_layout(db: &Connection) -> Result<bool> {
    if !history_layout(db) {
        return Ok(false);
    }
    if schema_version(db)? >= 3
        && (db
            .prepare("SELECT publication_seq FROM views LIMIT 0")
            .is_err()
            || db.prepare("SELECT item_id FROM note_ops LIMIT 0").is_err())
    {
        return Ok(false);
    }
    for sql in [
        "SELECT item_id,source_id,native_id FROM items LIMIT 0",
        "SELECT vrow,content_sha256,normalized_text FROM item_versions LIMIT 0",
        "SELECT observation_id,version_id,worktree_key,origin,json FROM observations LIMIT 0",
        "SELECT worktree_key,source_id,config_json FROM sources LIMIT 0",
        "SELECT key,view_id,json FROM result_sets LIMIT 0",
        "SELECT project_id,op,key,note_id,semantic_sha256,state,worktree,path,note_json FROM note_ops LIMIT 0",
        "SELECT source_id,source_path,json FROM cursors LIMIT 0",
        "SELECT from_version_id,field,to_item_id,json FROM relations LIMIT 0",
        "SELECT version_id,model,dims,preprocessing_version,generation_id,state,vector_json FROM vector_meta LIMIT 0",
        "SELECT version_id FROM embed_queue LIMIT 0",
        "SELECT rowid,normalized_text FROM items_fts LIMIT 0",
        "SELECT rowid,redacted_text FROM items_trigram LIMIT 0",
    ] {
        if db.prepare(sql).is_err() {
            return Ok(false);
        }
    }
    for (kind, name) in [
        ("trigger", "immutable_versions"),
        ("trigger", "immutable_observations"),
        ("trigger", "versions_ai"),
        ("trigger", "versions_ad"),
        ("index", "one_current_view"),
    ] {
        let present: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type=?1 AND name=?2)",
            params![kind, name],
            |r| r.get(0),
        )?;
        if !present {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn migrate_index(db: &Connection) -> Result<()> {
    let version = schema_version(db)?;
    if version >= SCHEMA && !has_table(db, "membership")? {
        return Ok(());
    }
    let tx = db.unchecked_transaction()?;
    if version == 1 {
        // Early v1 linked cached search results to the active generation's views.
        // Retained views now remain usable across generation repair.
        tx.execute_batch(
            "ALTER TABLE result_sets RENAME TO result_sets_v1;
             CREATE TABLE result_sets(key TEXT PRIMARY KEY,view_id TEXT NOT NULL,json TEXT NOT NULL);
             INSERT INTO result_sets SELECT key,view_id,json FROM result_sets_v1;
             DROP TABLE result_sets_v1;"
        )?;
    }
    if tx
        .prepare("SELECT publication_seq FROM views LIMIT 0")
        .is_err()
    {
        tx.execute_batch("ALTER TABLE views ADD COLUMN publication_seq INTEGER;")?;
    }
    if tx.prepare("SELECT item_id FROM note_ops LIMIT 0").is_err() {
        tx.execute_batch("ALTER TABLE note_ops ADD COLUMN item_id TEXT;")?;
    }
    // Eligibility and retained occurrences live in view_items. This obsolete
    // projection has no readers and must not make a usable index need repair.
    tx.execute_batch("DROP TABLE IF EXISTS membership;")?;
    tx.pragma_update(None, "user_version", SCHEMA)?;
    tx.commit()?;
    Ok(())
}

pub(super) fn migrate_access(db: &Connection) -> Result<()> {
    if schema_version(db)? <= SCHEMA {
        db.execute_batch(
                "BEGIN IMMEDIATE;
                 CREATE TABLE IF NOT EXISTS tombstones(identity_hash TEXT PRIMARY KEY, json TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS purge_progress(identity_hash TEXT PRIMARY KEY, epoch TEXT NOT NULL, complete INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS frecency(item_id TEXT PRIMARY KEY, count INTEGER NOT NULL, last_at TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS remote_observations(scope TEXT PRIMARY KEY, json TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS source_inventory(worktree TEXT PRIMARY KEY, had_items INTEGER NOT NULL);
                 CREATE TABLE IF NOT EXISTS forgotten_items(item_id TEXT PRIMARY KEY);
                 CREATE TABLE IF NOT EXISTS identity(key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE IF NOT EXISTS forgotten_note_ops(
                     project_id TEXT NOT NULL,key TEXT NOT NULL,item_id TEXT NOT NULL,
                     semantic_sha256 TEXT NOT NULL,legacy_key INTEGER NOT NULL,
                     PRIMARY KEY(project_id,key));
                 CREATE TABLE IF NOT EXISTS publication_clock(
                     singleton INTEGER PRIMARY KEY CHECK(singleton=1),sequence INTEGER NOT NULL);
                 INSERT OR IGNORE INTO publication_clock VALUES(1,0);
                 PRAGMA user_version=3;
                 COMMIT;"
            )?;
    }
    Ok(())
}

pub(super) fn create_schema(db: &Connection, clone_id: &str) -> Result<()> {
    let version: String = db.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    let parts: Vec<u32> = version.split('.').filter_map(|s| s.parse().ok()).collect();
    if parts.as_slice() < [3, 42, 0].as_slice() {
        return Err(Error::new(
            "unsupported_sqlite",
            "SQLite 3.42 or newer with FTS5 is required",
        ));
    }
    #[cfg(debug_assertions)]
    if std::env::var("MEMQ_FAULT").as_deref() == Ok("unsupported_fts") {
        return Err(Error::new(
            "unsupported_sqlite",
            "controlled FTS5-unavailable fixture",
        ));
    }
    db.execute_batch(
        "BEGIN IMMEDIATE;
        CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
        CREATE TABLE sources(worktree_key TEXT NOT NULL,source_id TEXT NOT NULL,config_json TEXT NOT NULL,PRIMARY KEY(worktree_key,source_id));
        CREATE TABLE items(item_id TEXT PRIMARY KEY,source_id TEXT NOT NULL,native_id TEXT NOT NULL);
        CREATE TABLE item_versions(
            vrow INTEGER PRIMARY KEY,version_id TEXT NOT NULL UNIQUE,item_id TEXT NOT NULL REFERENCES items(item_id) ON DELETE CASCADE,
            content_sha256 TEXT NOT NULL,payload_json TEXT NOT NULL,redacted_text TEXT NOT NULL,normalized_text TEXT NOT NULL);
        CREATE TRIGGER immutable_versions BEFORE UPDATE ON item_versions BEGIN SELECT RAISE(ABORT,'immutable version'); END;
        CREATE TABLE observations(observation_id TEXT PRIMARY KEY,version_id TEXT NOT NULL REFERENCES item_versions(version_id) ON DELETE CASCADE,
            worktree_key TEXT NOT NULL,origin TEXT NOT NULL,json TEXT NOT NULL);
        CREATE INDEX observations_versions ON observations(version_id,worktree_key,origin);
        CREATE TRIGGER immutable_observations BEFORE UPDATE ON observations BEGIN SELECT RAISE(ABORT,'immutable observation'); END;
        CREATE VIRTUAL TABLE items_fts USING fts5(normalized_text,content=item_versions,content_rowid=vrow,tokenize='unicode61 remove_diacritics 2');
        CREATE VIRTUAL TABLE items_trigram USING fts5(redacted_text,content=item_versions,content_rowid=vrow,tokenize='trigram',detail=full);
        INSERT INTO items_fts(items_fts,rank) VALUES('secure-delete',1);
        INSERT INTO items_trigram(items_trigram,rank) VALUES('secure-delete',1);
        CREATE TRIGGER versions_ai AFTER INSERT ON item_versions BEGIN
            INSERT INTO items_fts(rowid,normalized_text) VALUES(new.vrow,new.normalized_text);
            INSERT INTO items_trigram(rowid,redacted_text) VALUES(new.vrow,new.redacted_text);
        END;
        CREATE TRIGGER versions_ad AFTER DELETE ON item_versions BEGIN
            INSERT INTO items_fts(items_fts,rowid,normalized_text) VALUES('delete',old.vrow,old.normalized_text);
            INSERT INTO items_trigram(items_trigram,rowid,redacted_text) VALUES('delete',old.vrow,old.redacted_text);
        END;
        CREATE TABLE views(view_id TEXT PRIMARY KEY,scope_hash TEXT NOT NULL,inputs_hash TEXT NOT NULL,meta_json TEXT NOT NULL,is_current INTEGER NOT NULL,publication_seq INTEGER);
        CREATE UNIQUE INDEX one_current_view ON views(scope_hash) WHERE is_current=1;
        CREATE TABLE view_items(view_id TEXT NOT NULL REFERENCES views(view_id),member_seq TEXT NOT NULL,item_id TEXT NOT NULL,
            version_id TEXT NOT NULL REFERENCES item_versions(version_id) ON DELETE CASCADE,
            observation_id TEXT NOT NULL REFERENCES observations(observation_id) ON DELETE CASCADE,
            origin TEXT NOT NULL,branch TEXT NOT NULL,member_json TEXT NOT NULL,
            PRIMARY KEY(view_id,member_seq),UNIQUE(view_id,item_id,origin,branch));
        CREATE TABLE relations(from_version_id TEXT NOT NULL REFERENCES item_versions(version_id) ON DELETE CASCADE,field TEXT NOT NULL,to_item_id TEXT NOT NULL,json TEXT NOT NULL,
            PRIMARY KEY(from_version_id,field,to_item_id));
        CREATE TABLE vector_meta(version_id TEXT PRIMARY KEY REFERENCES item_versions(version_id) ON DELETE CASCADE,
            model TEXT NOT NULL,dims INTEGER NOT NULL,preprocessing_version TEXT NOT NULL,generation_id TEXT NOT NULL,state TEXT NOT NULL,vector_json TEXT NOT NULL);
        CREATE TABLE embed_queue(version_id TEXT PRIMARY KEY REFERENCES item_versions(version_id) ON DELETE CASCADE);
        CREATE TABLE cursors(source_id TEXT NOT NULL,source_path TEXT NOT NULL,json TEXT NOT NULL,PRIMARY KEY(source_id,source_path));
        CREATE TABLE result_sets(key TEXT PRIMARY KEY,view_id TEXT NOT NULL,json TEXT NOT NULL);
        CREATE TABLE note_ops(project_id TEXT NOT NULL,op TEXT NOT NULL,key TEXT NOT NULL,note_id TEXT NOT NULL,semantic_sha256 TEXT NOT NULL,
            state TEXT NOT NULL,worktree TEXT NOT NULL,path TEXT NOT NULL,note_json TEXT NOT NULL,item_id TEXT,PRIMARY KEY(project_id,op,key));
        PRAGMA user_version=3;
        COMMIT;"
    )?;
    for (key, value) in [
        ("clone_id", clone_id),
        ("sqlite_version", &version),
        ("memq_version", env!("CARGO_PKG_VERSION")),
    ] {
        db.execute("INSERT INTO meta VALUES(?1,?2)", params![key, value])?;
    }
    Ok(())
}

pub(super) fn sqlite_info(db: &Connection) -> Result<Value> {
    Ok(json!({
        "version": db.query_row("SELECT sqlite_version()",[],|r|r.get::<_,String>(0))?,
        "schema": schema_version(db)?,
        "fts5": true
    }))
}
