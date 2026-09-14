mod support;
use serde_json::json;
use support::Repo;

#[test]
fn immutable_versions_and_observations_survive_head_changes() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"item","status":"pending","decider":"owner"}]));
    r.commit("records");
    let first = r.ok(&["brief"]);
    r.write("seed.txt", "New code evidence\n");
    r.commit("code changed");
    let second = r.ok(&["brief"]);
    assert_eq!(
        first["items"][0]["version_id"],
        second["items"][0]["version_id"]
    );
    assert_ne!(
        first["items"][0]["observation"]["commit"],
        second["items"][0]["observation"]["commit"]
    );
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    assert!(
        db.execute("UPDATE item_versions SET redacted_text='changed'", [])
            .is_err()
    );
    assert!(db.execute("UPDATE observations SET json='{}'", []).is_err());
    let old = memq::store::load_view(&db, first["freshness"]["view_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        old.members[0].observation.commit.as_deref(),
        first["items"][0]["observation"]["commit"].as_str()
    );
    r.ok(&["doctor"]);
}

#[test]
fn interrupted_publication_preserves_previous_view() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"before"}]));
    let first = r.ok(&["brief"]);
    r.records(json!([{"id":"one","text":"after"}]));
    let out = r
        .command()
        .env("MEMQ_FAULT", "before_publish")
        .arg("brief")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(86));
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM item_versions WHERE redacted_text LIKE '%after%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let count: i64 = db
        .query_row(
            "SELECT count(*) FROM views WHERE view_id=?1",
            [first["freshness"]["view_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    drop(db);
    assert_ne!(
        r.ok(&["brief"])["freshness"]["view_id"],
        first["freshness"]["view_id"]
    );
}

#[test]
fn edits_inside_publication_roll_back_before_retry() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"stable before"}]));
    r.ok(&["brief"]);
    r.records(json!([{"id":"one","text":"obsolete intermediate"}]));
    let ready = r.temp.path().join("publishing");
    let child = r
        .command()
        .env("MEMQ_PAUSE_AT", "before_publish")
        .env("MEMQ_PAUSE_FILE", &ready)
        .arg("brief")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        ready.exists(),
        "publication did not reach the transaction witness"
    );
    r.records(json!([{"id":"one","text":"latest accepted snapshot"}]));
    std::fs::remove_file(&ready).unwrap();
    // Each retry reaches the same pause hook; release it until the process exits.
    let mut child = child;
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if ready.exists() {
            std::fs::remove_file(&ready).unwrap();
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "publication did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let text: String = db
        .query_row(
            "SELECT group_concat(redacted_text) FROM item_versions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!text.contains("obsolete intermediate"));
    assert!(text.contains("latest accepted snapshot"));
}

#[test]
fn historical_view_and_original_pointer_survive_a_rebuild() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"original evidence"}]));
    let first = r.ok(&["brief"]);
    let item = &first["items"][0];
    r.records(json!([{"id":"one","text":"replacement evidence"}]));
    r.ok(&["rebuild"]);
    let old = r.ok(&[
        "show",
        item["id"].as_str().unwrap(),
        "--view-id",
        first["freshness"]["view_id"].as_str().unwrap(),
    ]);
    assert!(
        old["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("original evidence")
    );
    assert_eq!(old["items"][0]["pointer"], item["pointer"]);
    assert_eq!(old["freshness"]["status"], "stale");
}
