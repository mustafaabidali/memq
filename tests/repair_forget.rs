mod support;
use serde_json::json;
use support::Repo;

#[test]
fn brief_repairs_deleted_and_corrupt_indexes_without_authored_changes() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"record","text":"Surviving evidence"}]));
    let original = std::fs::read(r.root.join("records.json")).unwrap();
    let first = r.ok(&["brief"]);
    let path = r.db_path();
    std::fs::remove_file(&path).unwrap();
    let repaired = r.ok(&["brief"]);
    assert_eq!(first["items"][0]["id"], repaired["items"][0]["id"]);
    let path = r.db_path();
    std::fs::write(&path, b"not a SQLite database").unwrap();
    let repaired = r.ok(&["brief"]);
    assert_eq!(repaired["items"][0]["text"], first["items"][0]["text"]);
    assert_eq!(
        std::fs::read(r.root.join("records.json")).unwrap(),
        original
    );
}

#[test]
fn newer_schema_never_downgrades_or_rebuilds() {
    let r = Repo::initialized();
    let path = r.db_path();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=999").unwrap();
    drop(db);
    r.error(&["brief"], "unsupported_schema");
    r.error(&["rebuild"], "unsupported_schema");
    assert_eq!(
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}

#[test]
fn legacy_access_schema_migrates_and_missing_index_tables_repair_automatically() {
    let r = Repo::initialized();
    let note = r.ok(&[
        "note",
        "--text",
        "Keep authored progress",
        "--idempotency-key",
        "legacy",
    ]);
    let original = std::fs::read(r.root.join(note["path"].as_str().unwrap())).unwrap();
    let old_view = r.ok(&["brief"]);
    let access_path = r.store_root().join("access.sqlite");
    let access = rusqlite::Connection::open(&access_path).unwrap();
    access
        .execute_batch("DROP TABLE identity; DROP TABLE forgotten_items; PRAGMA user_version=1;")
        .unwrap();
    drop(access);
    let old_path = r.db_path();
    let db = rusqlite::Connection::open(&old_path).unwrap();
    db.execute_batch("PRAGMA user_version=1;").unwrap();
    drop(db);
    let upgraded = r.ok(&["brief"]);
    assert_eq!(upgraded["items"][0]["id"], note["id"]);
    assert_eq!(
        rusqlite::Connection::open(&access_path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        memq::store::SCHEMA
    );
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    db.execute_batch("DROP TABLE vector_meta;").unwrap();
    drop(db);
    let repaired = r.ok(&["brief"]);
    assert_ne!(r.db_path(), old_path);
    assert_eq!(repaired["items"][0]["id"], note["id"]);
    let shown = r.ok(&[
        "show",
        note["id"].as_str().unwrap(),
        "--view-id",
        old_view["freshness"]["view_id"].as_str().unwrap(),
    ]);
    assert!(
        shown["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Keep authored progress")
    );
    assert_eq!(
        std::fs::read(r.root.join(note["path"].as_str().unwrap())).unwrap(),
        original
    );
    assert_eq!(
        r.ok(&[
            "note",
            "--text",
            "Keep authored progress",
            "--idempotency-key",
            "legacy"
        ])["id"],
        note["id"]
    );
}

#[test]
fn newer_access_schema_is_not_modified_and_missing_deletion_ledger_fails_closed() {
    let r = Repo::initialized();
    let path = r.store_root().join("access.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=999;").unwrap();
    drop(db);
    r.error(&["brief"], "unsupported_schema");
    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
    db.execute_batch("PRAGMA user_version=2; DROP TABLE tombstones;")
        .unwrap();
    drop(db);
    r.error(&["brief"], "access_state_incomplete");
}

#[test]
fn tombstone_survives_replay_index_loss_and_branch_removal() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"forget","text":"Erase retained private-like fixture"}]));
    let b = r.ok(&["brief"]);
    let id = b["items"][0]["id"].as_str().unwrap();
    r.ok(&["rebuild"]);
    r.ok(&["forget", id]);
    let shown = r.ok(&["show", id]);
    assert_eq!(
        shown["items"][0],
        json!({"id":id,"availability":"forgotten"})
    );
    std::fs::remove_file(r.db_path()).unwrap();
    assert!(r.ok(&["brief"])["items"].as_array().unwrap().is_empty());
    std::fs::remove_dir_all(r.root.join(".memq/tombstones")).unwrap();
    assert!(r.ok(&["brief"])["items"].as_array().unwrap().is_empty());
    for entry in std::fs::read_dir(r.store_root().join("generations")).unwrap() {
        let p = entry.unwrap().path().join("index.sqlite");
        if p.exists() {
            let db = rusqlite::Connection::open(p).unwrap();
            assert_eq!(
                db.query_row("SELECT count(*) FROM item_versions", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[test]
fn source_loss_is_reported_and_note_retry_backfills_after_rebuild() {
    let r = Repo::initialized();
    let n = r.ok(&[
        "note",
        "--text",
        "Keep durable note",
        "--idempotency-key",
        "backfill",
    ]);
    r.ok(&["rebuild"]);
    assert_eq!(
        n["id"],
        r.ok(&[
            "note",
            "--text",
            "Keep durable note",
            "--idempotency-key",
            "backfill"
        ])["id"]
    );
    r.record_source();
    r.records(json!([{"id":"one","text":"source"}]));
    r.ok(&["brief"]);
    std::fs::remove_file(r.root.join("records.json")).unwrap();
    std::fs::remove_dir_all(r.root.join(".memq/notes")).unwrap();
    std::fs::remove_file(r.db_path()).unwrap();
    r.error(&["brief"], "recovery_limit_reached");
}

#[test]
fn notes_only_context_loss_is_disclosed_before_and_after_index_loss() {
    let r = Repo::initialized();
    r.ok(&[
        "note",
        "--text",
        "Only surviving project context",
        "--idempotency-key",
        "only",
    ]);
    let b = r.ok(&["brief"]);
    std::fs::remove_dir_all(r.root.join(".memq/notes")).unwrap();
    let stale = r.ok(&["brief"]);
    assert_eq!(stale["freshness"]["view_id"], b["freshness"]["view_id"]);
    assert_eq!(stale["freshness"]["status"], "stale");
    assert_eq!(stale["reason"], "recovery_limit_reached");
    assert!(
        !stale["coverage"]["sources_missing"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(r.store_root().join("generations")).unwrap();
    r.error(&["brief"], "recovery_limit_reached");
}

#[test]
fn forget_repairs_old_fts_indexes_and_preserves_other_historical_evidence() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"forget","text":"Remove retained fixture evidence"},
        {"id":"keep","text":"Keep this original historical evidence"}
    ]));
    let original = std::fs::read(r.root.join("records.json")).unwrap();
    let brief = r.ok(&["brief"]);
    let item = brief["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["native_id"] == "forget")
        .unwrap();
    let id = item["id"].as_str().unwrap();
    let keep_id = brief["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["native_id"] == "keep")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let old_path = r.db_path();
    let db = rusqlite::Connection::open(&old_path).unwrap();
    db.execute_batch("DROP TABLE items_trigram;").unwrap();
    drop(db);
    r.ok(&["brief"]);
    assert_ne!(r.db_path(), old_path);
    r.ok(&["forget", id]);
    assert!(
        old_path.exists(),
        "readable historical evidence must survive"
    );
    let db = rusqlite::Connection::open(&old_path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM item_versions WHERE item_id=?1",
            [id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM items_trigram WHERE items_trigram MATCH '\"retained\"'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    assert_eq!(r.ok(&["show", id])["items"][0]["availability"], "forgotten");
    r.ok(&["rebuild"]);
    let shown = r.ok(&[
        "show",
        keep_id,
        "--view-id",
        brief["freshness"]["view_id"].as_str().unwrap(),
    ]);
    assert!(
        shown["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Keep this original historical evidence")
    );
    assert_eq!(
        std::fs::read(r.root.join("records.json")).unwrap(),
        original
    );
}

#[test]
fn forget_never_removes_a_newer_archived_schema() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"forget","text":"Newer schema fixture"}]));
    let brief = r.ok(&["brief"]);
    let id = brief["items"][0]["id"].as_str().unwrap();
    let old_path = r.db_path();
    r.ok(&["rebuild"]);
    let db = rusqlite::Connection::open(&old_path).unwrap();
    db.execute_batch("PRAGMA user_version=999;").unwrap();
    drop(db);
    r.error(&["forget", id], "unsupported_schema");
    assert_eq!(
        rusqlite::Connection::open(old_path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        999
    );
}
