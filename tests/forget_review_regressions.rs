mod support;

use serde_json::{Value, json};
use std::fs;
use support::Repo;

const BODY: &str = "Synthetic forgotten orchard inspection";
const KEY: &str = "orchard-inspection";

fn note(r: &Repo) -> Value {
    r.ok(&["note", "--text", BODY, "--idempotency-key", KEY])
}

fn assert_no_retained_body(r: &Repo) {
    for entry in fs::read_dir(r.store_root().join("generations")).unwrap() {
        let path = entry.unwrap().path().join("index.sqlite");
        if !path.exists() {
            continue;
        }
        let db = rusqlite::Connection::open(&path).unwrap();
        let retained: i64 = db
            .query_row(
                "SELECT count(*) FROM note_ops WHERE instr(note_json,?1)>0",
                [BODY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 0, "forgotten operation retained its body");
        let retained: i64 = db
            .query_row(
                "SELECT count(*) FROM item_versions WHERE instr(payload_json,?1)>0",
                [BODY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 0, "forgotten derived evidence retained its body");
        drop(db);
        assert!(
            !fs::read(path)
                .unwrap()
                .windows(BODY.len())
                .any(|bytes| bytes == BODY.as_bytes()),
            "forgotten bytes remain in the closed generation"
        );
    }
    assert!(
        !fs::read(r.store_root().join("access.sqlite"))
            .unwrap()
            .windows(BODY.len())
            .any(|bytes| bytes == BODY.as_bytes()),
        "persistent retry identity must be content-free"
    );
}

fn forgotten_note_lifecycle(indexed: bool) {
    let r = Repo::initialized();
    let first = note(&r);
    let path = r.root.join(first["path"].as_str().unwrap());
    let original = fs::read(&path).unwrap();
    if indexed {
        r.ok(&["brief"]);
    }
    r.ok(&["forget", first["id"].as_str().unwrap()]);
    assert_no_retained_body(&r);
    for rebuild in [false, true] {
        if rebuild {
            r.ok(&["rebuild"]);
        }
        assert_eq!(
            note(&r),
            json!({"id":first["id"],"availability":"forgotten"}),
            "a retry must resolve to the original forgotten identity"
        );
        assert!(r.ok(&["brief"])["items"].as_array().unwrap().is_empty());
        assert_no_retained_body(&r);
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(
            fs::read_dir(r.root.join(".memq/notes")).unwrap().count(),
            1,
            "retry created a second authored note"
        );
    }
}

#[test]
fn immediate_forget_purges_operation_body_and_keeps_retry_identity() {
    forgotten_note_lifecycle(false);
}

#[test]
fn indexed_forget_keeps_retry_identity_through_rebuild() {
    forgotten_note_lifecycle(true);
}

#[test]
fn forgotten_retry_survives_index_loss_and_absent_authored_note() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"anchor","text":"Surviving synthetic source"}]));
    let first = note(&r);
    r.ok(&["brief"]);
    r.ok(&["forget", first["id"].as_str().unwrap()]);
    fs::remove_file(r.root.join(first["path"].as_str().unwrap())).unwrap();
    fs::remove_dir_all(r.store_root().join("generations")).unwrap();
    assert_eq!(
        note(&r),
        json!({"id":first["id"],"availability":"forgotten"})
    );
    assert_eq!(fs::read_dir(r.root.join(".memq/notes")).unwrap().count(), 0);
    assert_no_retained_body(&r);
}

#[test]
fn source_forgetting_never_reserves_a_new_operation_body() {
    let r = Repo::initialized();
    r.ok(&["forget", "--source", "notes"]);
    let first = note(&r);
    assert_eq!(first["availability"], "forgotten");
    assert_no_retained_body(&r);
    assert_eq!(note(&r), first);
    r.ok(&["rebuild"]);
    assert_eq!(note(&r), first);
    assert_no_retained_body(&r);
    assert!(!r.root.join(".memq/notes").exists());
}

#[test]
fn interrupted_note_and_purge_retries_keep_the_original_forgotten_identity() {
    for fault in [
        "after_note_reservation",
        "after_note_tmp",
        "after_rename_before_stage",
    ] {
        let r = Repo::initialized();
        let out = r
            .command()
            .env("MEMQ_FAULT", fault)
            .args(["note", "--text", BODY, "--idempotency-key", KEY])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86));
        let db = rusqlite::Connection::open(r.db_path()).unwrap();
        let native: String = db
            .query_row("SELECT note_id FROM note_ops", [], |row| row.get(0))
            .unwrap();
        drop(db);
        let config = memq::config::Config::load(&r.root).unwrap();
        let id = memq::util::item_id(&config.project_id, "notes", &native);
        let out = r
            .command()
            .env("MEMQ_FAULT", "mid_purge")
            .args(["forget", &id])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86));
        assert_eq!(note(&r), json!({"id":id,"availability":"forgotten"}));
        r.ok(&["rebuild"]);
        assert_eq!(note(&r), json!({"id":id,"availability":"forgotten"}));
        assert_no_retained_body(&r);
        assert!(!r.root.join(format!(".memq/notes/.{native}.tmp")).exists());
        let final_file = r.root.join(format!(".memq/notes/{native}.json"));
        assert_eq!(final_file.exists(), fault == "after_rename_before_stage");
    }
}

#[test]
fn forgotten_key_still_rejects_different_semantics() {
    let r = Repo::initialized();
    let first = note(&r);
    r.ok(&["forget", first["id"].as_str().unwrap()]);
    r.ok(&["rebuild"]);
    r.error(
        &[
            "note",
            "--text",
            "A different synthetic inspection",
            "--idempotency-key",
            KEY,
        ],
        "idempotency_conflict",
    );
    assert_eq!(
        note(&r),
        json!({"id":first["id"],"availability":"forgotten"})
    );
    assert_no_retained_body(&r);
}

#[test]
fn legacy_unindexed_operation_recovers_its_configured_source_for_purge() {
    for scope in ["id", "source", "time"] {
        let r = Repo::initialized();
        let config = r.root.join(".memq/config.toml");
        let text = fs::read_to_string(&config)
            .unwrap()
            .replace("id = \"notes\"", "id = \"orchard-notes\"");
        fs::write(config, text).unwrap();
        r.ok(&["reconcile", "--allow-source-removal"]);
        let first = note(&r);
        let db = rusqlite::Connection::open(r.db_path()).unwrap();
        db.execute_batch("ALTER TABLE note_ops DROP COLUMN item_id; PRAGMA user_version=2;")
            .unwrap();
        drop(db);
        let args = match scope {
            "id" => vec!["forget", first["id"].as_str().unwrap()],
            "source" => vec!["forget", "--source", "orchard-notes"],
            _ => vec![
                "forget",
                "--source",
                "orchard-notes",
                "--after",
                "2026-09-13T19:00:00Z",
                "--before",
                "2026-09-13T21:00:00Z",
            ],
        };
        r.ok(&args);
        assert_no_retained_body(&r);
        r.ok(&["rebuild"]);
        assert_eq!(
            note(&r),
            json!({"id":first["id"],"availability":"forgotten"})
        );
    }
}

#[test]
fn missing_persistent_note_identity_table_fails_closed() {
    let r = Repo::initialized();
    let first = note(&r);
    r.ok(&["forget", first["id"].as_str().unwrap()]);
    let path = r.store_root().join("access.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("DROP TABLE forgotten_note_ops;").unwrap();
    drop(db);
    r.error(
        &["note", "--text", BODY, "--idempotency-key", KEY],
        "access_state_incomplete",
    );
    let db = rusqlite::Connection::open(path).unwrap();
    assert!(
        db.prepare("SELECT * FROM forgotten_note_ops").is_err(),
        "opening silently recreated persistent identity state"
    );
}

#[test]
fn conflicting_authored_keys_from_clones_do_not_block_an_unrelated_note() {
    let r = Repo::initialized();
    r.commit("synthetic shared configuration");
    let other = r.clone_repo();
    let first = r.ok(&[
        "note",
        "--text",
        "First clone inspection",
        "--idempotency-key",
        "shared-step",
    ]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_memq"))
        .arg("--repo")
        .arg(&other)
        .env("MEMQ_DATA_DIR", &r.data)
        .env("MEMQ_NOW", "2026-09-13T20:00:00Z")
        .args([
            "note",
            "--text",
            "Second clone inspection",
            "--idempotency-key",
            "shared-step",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let second: Value = serde_json::from_slice(&out.stdout).unwrap();
    let second_path = second["path"].as_str().unwrap();
    r.write(second_path, fs::read(other.join(second_path)).unwrap());
    let originals: Vec<_> = [&first, &second]
        .into_iter()
        .map(|note| {
            let path = r.root.join(note["path"].as_str().unwrap());
            (path.clone(), fs::read(path).unwrap())
        })
        .collect();
    for rebuild in [false, true] {
        if rebuild {
            r.ok(&["rebuild"]);
        }
        let saved = r.ok(&[
            "note",
            "--text",
            "An unrelated orchard inspection",
            "--idempotency-key",
            "unrelated-step",
        ]);
        assert_eq!(saved["durability"], "durable");
        let duplicates = saved["duplicates"]
            .as_array()
            .expect("conflicting notes must be reported");
        assert_eq!(duplicates.len(), 1);
        let mut ids = duplicates[0]["note_ids"].as_array().unwrap().clone();
        ids.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        let mut expected = vec![first["note_id"].clone(), second["note_id"].clone()];
        expected.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        assert_eq!(ids, expected);
        let conflict = r.error(
            &[
                "note",
                "--text",
                "First clone inspection",
                "--idempotency-key",
                "shared-step",
            ],
            "idempotency_conflict",
        );
        assert_eq!(conflict["error"]["detail"]["note_ids"], json!(expected));
        for (path, bytes) in &originals {
            assert_eq!(&fs::read(path).unwrap(), bytes);
        }
    }
    assert_eq!(fs::read_dir(r.root.join(".memq/notes")).unwrap().count(), 3);
}

fn vector_fixture() -> (Repo, String) {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"keep","text":"cobalt"},
        {"id":"discard","text":"Synthetic forgotten vector"}
    ]));
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/embedding.py");
    r.add_source(&format!(
        "[vectors]\ncommand=['python3',{}]\nmodel='test-plumbing-only'\ndims=4\npreprocessing_version='fixture-v1'\ntimeout_seconds=1\n",
        json!(script)
    ));
    r.ok(&["embed"]);
    let before = r.ok(&["brief"]);
    let id = before["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "discard")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    r.ok(&["search", "quartz", "--budget", "10000"]);
    (r, id)
}

fn vector_caches(r: &Repo) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut caches = std::collections::BTreeMap::new();
    for directory in fs::read_dir(r.store_root().join("generations")).unwrap() {
        for entry in fs::read_dir(directory.unwrap().path()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "usearch") {
                caches.insert(path.clone(), fs::read(path).unwrap());
            }
        }
    }
    caches
}

#[test]
fn completed_tombstones_do_not_invalidate_safe_vector_caches_again() {
    let (r, id) = vector_fixture();
    assert!(!vector_caches(&r).is_empty());
    r.ok(&["forget", &id]);
    assert!(vector_caches(&r).is_empty());
    r.ok(&["search", "quartz", "--budget", "10000"]);
    let caches = vector_caches(&r);
    assert!(!caches.is_empty());
    r.ok(&["brief"]);
    assert_eq!(
        vector_caches(&r),
        caches,
        "an unchanged tombstone removed safe caches"
    );
    r.ok(&["search", "quartz", "--budget", "10000"]);
    assert_eq!(vector_caches(&r), caches);
}

#[test]
fn interrupted_vector_cleanup_resumes_after_the_rows_are_already_gone() {
    let (r, id) = vector_fixture();
    let caches = vector_caches(&r);
    assert!(!caches.is_empty());
    let temporary_cache = caches.keys().next().unwrap().with_extension("tmp");
    fs::copy(caches.keys().next().unwrap(), &temporary_cache).unwrap();
    let out = r
        .command()
        .env("MEMQ_FAULT", "before_vector_purge")
        .args(["forget", &id])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(86));
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM item_versions WHERE item_id=?1",
            [&id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    assert_eq!(
        vector_caches(&r),
        caches,
        "fault did not precede cache cleanup"
    );
    assert!(temporary_cache.exists());
    r.ok(&["brief"]);
    assert!(vector_caches(&r).is_empty());
    assert!(!temporary_cache.exists());
    r.ok(&["search", "quartz", "--budget", "10000"]);
    let safe = vector_caches(&r);
    assert!(!safe.is_empty());
    r.ok(&["brief"]);
    assert_eq!(vector_caches(&r), safe);
}

#[test]
fn old_tombstones_mark_newly_discovered_rows_pending_before_deleting_them() {
    let (r, id) = vector_fixture();
    let retained = r.temp.path().join("retained-before-purge.sqlite");
    fs::copy(r.db_path(), &retained).unwrap();
    r.ok(&["forget", &id]);
    r.ok(&["search", "quartz", "--budget", "10000"]);
    let safe = vector_caches(&r);
    assert!(!safe.is_empty());
    let directory = r
        .store_root()
        .join("generations/00000000000000000000000005");
    fs::create_dir(&directory).unwrap();
    let archive = directory.join("index.sqlite");
    fs::copy(retained, &archive).unwrap();
    fs::write(directory.join("VERIFIED"), b"1").unwrap();
    let out = r
        .command()
        .env("MEMQ_FAULT", "before_vector_purge")
        .arg("brief")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(86));
    let db = rusqlite::Connection::open(archive).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM item_versions WHERE item_id=?1",
            [&id],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    drop(db);
    assert_eq!(vector_caches(&r), safe);
    r.ok(&["brief"]);
    assert!(
        vector_caches(&r).is_empty(),
        "pending cleanup was lost with the deleted rows"
    );
}
