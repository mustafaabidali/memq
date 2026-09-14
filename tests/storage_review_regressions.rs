mod support;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use support::Repo;

fn read_page(r: &Repo, args: &[&str]) -> Value {
    let out = r.run(args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn fresh_show_preserves_occurrences(compact: bool) {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([]));
    r.commit("synthetic source configuration");
    let branches: Vec<String> = (0..8).map(|n| format!("orchard-{n}")).collect();
    for branch in &branches {
        r.git(&["checkout", "-q", "-b", branch, "main"]);
        let text = format!("{branch} apple pear plum\n").repeat(400);
        r.records(json!([{"id":"shared-inspection","text":text}]));
        r.commit("synthetic branch evidence");
    }
    r.git(&["checkout", "-q", "main"]);
    let config = memq::config::Config::load(&r.root).unwrap();
    let id = memq::util::item_id(&config.project_id, "records", "shared-inspection");
    let branch_arg = branches.join(",");
    let mut args = vec![
        "show",
        &id,
        "--branches",
        &branch_arg,
        "--budget-kind",
        "bytes",
        "--budget",
        "4800",
    ];
    if compact {
        args.push("--compact");
    }
    // The first page must construct its view. Starting with --view-id would
    // miss the transition from the fresh vector to persisted member ordering.
    let mut page = read_page(&r, &args);
    let view = page["freshness"]["view_id"].as_str().unwrap().to_owned();
    assert_eq!(
        support::expanded_items(&page)[0]["observation"]["branch"],
        branches[0]
    );
    assert!(page["continuation"].is_string());
    let all = read_page(
        &r,
        &[
            "show",
            &id,
            "--branches",
            &branch_arg,
            "--view-id",
            &view,
            "--budget-kind",
            "bytes",
            "--budget",
            "200000",
        ],
    );
    let originals: BTreeMap<String, Value> = all["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["observation"]["branch"].as_str().unwrap().to_owned(),
                item.clone(),
            )
        })
        .collect();
    assert_eq!(originals.len(), branches.len());
    let mut restored = String::new();
    let mut occurrence = 0;
    let mut pages = 0;
    loop {
        assert_eq!(page["freshness"]["view_id"], view);
        for item in support::expanded_items(&page) {
            assert!(occurrence < branches.len(), "duplicated an occurrence");
            let branch = &branches[occurrence];
            assert_eq!(
                item["observation"]["branch"], *branch,
                "continuation changed the canonical occurrence order"
            );
            assert_eq!(item["text_range"][0], restored.len());
            restored.push_str(item["text"].as_str().unwrap());
            assert_eq!(item["text_range"][1], restored.len());
            assert_eq!(item["pointer"], originals[branch]["pointer"]);
            if item["complete"] == true {
                assert_eq!(restored, originals[branch]["text"].as_str().unwrap());
                assert_eq!(item["total_bytes"], restored.len());
                restored.clear();
                occurrence += 1;
            }
        }
        pages += 1;
        assert!(pages < 150, "pagination did not progress");
        let Some(next) = page["continuation"].as_str() else {
            break;
        };
        let mut next_args = args.clone();
        next_args.extend(["--continuation", next]);
        page = read_page(&r, &next_args);
    }
    assert!(pages > branches.len());
    assert_eq!(occurrence, branches.len(), "lost a branch occurrence");
    assert!(restored.is_empty());
}

#[test]
fn fresh_full_show_preserves_occurrences_across_continuations() {
    fresh_show_preserves_occurrences(false);
}

#[test]
fn fresh_compact_show_preserves_occurrences_across_continuations() {
    fresh_show_preserves_occurrences(true);
}

fn missing_ledger_fails_closed(scope: &str, remove_marker: bool) {
    let r = Repo::initialized();
    let note = r.ok(&[
        "note",
        "--text",
        "Synthetic ledger-loss evidence",
        "--idempotency-key",
        "ledger-loss",
    ]);
    r.ok(&["brief"]);
    r.ok(&["rebuild"]);
    let args = match scope {
        "id" => vec!["forget", note["id"].as_str().unwrap()],
        "source" => vec!["forget", "--source", "notes"],
        "project" => vec!["forget", "--project"],
        "time" => vec![
            "forget",
            "--source",
            "notes",
            "--after",
            "2026-09-13T19:00:00Z",
            "--before",
            "2026-09-13T21:00:00Z",
        ],
        _ => unreachable!(),
    };
    r.ok(&args);
    fs::remove_dir_all(r.root.join(".memq/tombstones")).unwrap();
    assert!(r.ok(&["brief"])["items"].as_array().unwrap().is_empty());
    let root = r.store_root();
    let mut retained = BTreeMap::new();
    for entry in fs::read_dir(root.join("generations")).unwrap() {
        let path = entry.unwrap().path().join("index.sqlite");
        retained.insert(path.clone(), fs::read(path).unwrap());
    }
    assert!(retained.len() >= 2);
    let marker = fs::read(root.join("CURRENT")).unwrap();
    if remove_marker {
        fs::remove_file(root.join("CURRENT")).unwrap();
    }
    let ledger = root.join("access.sqlite");
    fs::remove_file(&ledger).unwrap();
    for command in ["brief", "rebuild"] {
        r.error(&[command], "access_state_incomplete");
        assert!(!ledger.exists(), "opening manufactured an empty ledger");
        if remove_marker {
            assert!(!root.join("CURRENT").exists());
        } else {
            assert_eq!(fs::read(root.join("CURRENT")).unwrap(), marker);
        }
        assert_eq!(
            fs::read_dir(root.join("generations")).unwrap().count(),
            retained.len()
        );
        for (path, bytes) in &retained {
            assert_eq!(&fs::read(path).unwrap(), bytes);
        }
    }
}

#[test]
fn missing_access_file_blocks_replay_of_forgotten_id() {
    missing_ledger_fails_closed("id", false);
}

#[test]
fn missing_access_file_blocks_replay_of_forgotten_source() {
    missing_ledger_fails_closed("source", false);
}

#[test]
fn missing_access_file_blocks_replay_of_forgotten_project() {
    missing_ledger_fails_closed("project", false);
}

#[test]
fn missing_access_file_blocks_replay_of_forgotten_time_range() {
    missing_ledger_fails_closed("time", false);
}

#[test]
fn surviving_generations_require_the_ledger_even_without_current() {
    missing_ledger_fails_closed("id", true);
}

fn history_across_generations(legacy: bool, remove_baselines: bool) {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"inspection","text":"First synthetic evidence"},
        {"id":"anchor","text":"Surviving source"}
    ]));
    let first = r.ok(&["brief"]);
    let first_item = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "inspection")
        .unwrap();
    let id = first_item["id"].as_str().unwrap();
    let first_directory = r.db_path().parent().unwrap().to_owned();
    r.records(json!([
        {"id":"inspection","text":"Second synthetic evidence"},
        {"id":"anchor","text":"Surviving source"}
    ]));
    r.ok(&["rebuild"]);
    let second = r.ok(&["brief"]);
    let second_item = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == id)
        .unwrap();
    let second_directory = r.db_path().parent().unwrap().to_owned();
    r.records(json!([{"id":"anchor","text":"Surviving source"}]));
    r.ok(&["rebuild"]);
    let absent = r.ok(&["brief"]);
    r.records(json!([
        {"id":"inspection","text":"Future synthetic evidence"},
        {"id":"anchor","text":"Surviving source"}
    ]));
    r.ok(&["rebuild"]);
    let root = r.store_root();
    // Names deliberately disagree with generation publication order. All
    // public commands also have the same frozen publication timestamp.
    fs::rename(
        first_directory,
        root.join("generations/7ZZZZZZZZZZZZZZZZZZZZZZZZZ"),
    )
    .unwrap();
    fs::rename(
        second_directory,
        root.join("generations/00000000000000000000000001"),
    )
    .unwrap();
    if legacy {
        for entry in fs::read_dir(root.join("generations")).unwrap() {
            let db =
                rusqlite::Connection::open(entry.unwrap().path().join("index.sqlite")).unwrap();
            db.execute_batch(
                "ALTER TABLE views DROP COLUMN publication_seq; PRAGMA user_version=2;",
            )
            .unwrap();
            if remove_baselines {
                db.execute_batch("UPDATE views SET meta_json=json_set(meta_json,'$.changes_since.baseline',NULL);").unwrap();
            }
        }
        let access = rusqlite::Connection::open(root.join("access.sqlite")).unwrap();
        access.execute_batch(
            "DROP TABLE publication_clock; DROP TABLE forgotten_note_ops; PRAGMA user_version=2;"
        ).unwrap();
    }
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let _lock = memq::store::MutationLock::acquire(&root).unwrap();
    let store = memq::store::Store::open(&root, &repo.clone_id, false).unwrap();
    let history = store.history_at(id, absent["freshness"]["view_id"].as_str().unwrap());
    if remove_baselines {
        assert_eq!(history.unwrap_err().code, "history_order_unavailable");
        return;
    }
    let history = history.unwrap();
    assert!(!history.is_empty());
    assert_eq!(
        history[0].0.version_id,
        second_item["version_id"].as_str().unwrap()
    );
    assert!(
        history.iter().any(|(member, _, _)| {
            member.version_id == first_item["version_id"].as_str().unwrap()
        })
    );
    for (member, _, _) in history {
        assert!(
            member.version_id == first_item["version_id"].as_str().unwrap()
                || member.version_id == second_item["version_id"].as_str().unwrap(),
            "bounded history included a future version"
        );
    }
    assert_eq!(
        store
            .history_at(id, "00000000000000000000000000")
            .unwrap_err()
            .code,
        "stale_continuation"
    );
}

#[test]
fn bounded_history_uses_publication_sequence_across_generations() {
    history_across_generations(false, false);
}

#[test]
fn legacy_history_uses_recorded_baselines_across_generations() {
    history_across_generations(true, false);
}

#[test]
fn ambiguous_legacy_history_never_guesses_from_generation_names() {
    history_across_generations(true, true);
}

#[test]
fn a_legacy_baseline_does_not_certify_later_rows_in_its_generation() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"inspection","text":"Original synthetic evidence"},
        {"id":"anchor","text":"Surviving source"}
    ]));
    let first = r.ok(&["brief"]);
    let id = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "inspection")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let root = r.store_root();
    let first_generation = fs::read(root.join("CURRENT")).unwrap();
    r.records(json!([{"id":"anchor","text":"Surviving source"}]));
    r.ok(&["rebuild"]);
    let absent = r.ok(&["brief"]);
    // Model a replacement verified before an interrupted CURRENT switch.
    // The still-selected older generation can receive subsequent publications.
    fs::write(root.join("CURRENT"), first_generation).unwrap();
    r.records(json!([
        {"id":"inspection","text":"Later synthetic evidence in the original generation"},
        {"id":"anchor","text":"Surviving source"}
    ]));
    r.ok(&["brief"]);
    for entry in fs::read_dir(root.join("generations")).unwrap() {
        let db = rusqlite::Connection::open(entry.unwrap().path().join("index.sqlite")).unwrap();
        db.execute_batch("ALTER TABLE views DROP COLUMN publication_seq; PRAGMA user_version=2;")
            .unwrap();
    }
    let access = rusqlite::Connection::open(root.join("access.sqlite")).unwrap();
    access
        .execute_batch(
            "DROP TABLE publication_clock; DROP TABLE forgotten_note_ops; PRAGMA user_version=2;",
        )
        .unwrap();
    drop(access);
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let _lock = memq::store::MutationLock::acquire(&root).unwrap();
    let store = memq::store::Store::open(&root, &repo.clone_id, false).unwrap();
    assert_eq!(
        store
            .history_at(id, absent["freshness"]["view_id"].as_str().unwrap())
            .unwrap_err()
            .code,
        "history_order_unavailable"
    );
}

#[test]
fn an_empty_uncommitted_generation_does_not_block_tombstone_replay() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"discard","text":"Forgotten synthetic evidence"},
        {"id":"keep","text":"Retained synthetic evidence"}
    ]));
    let before = r.ok(&["brief"]);
    let forgotten = before["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "discard")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    r.ok(&["forget", forgotten]);
    let directory = r
        .store_root()
        .join("generations/00000000000000000000000003");
    fs::create_dir(&directory).unwrap();
    let db = rusqlite::Connection::open(directory.join("index.sqlite")).unwrap();
    db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
    drop(db);
    let after = r.ok(&["brief"]);
    assert_eq!(after["items"].as_array().unwrap().len(), 1);
    assert_eq!(after["items"][0]["native_id"], "keep");
    assert!(
        !directory.exists(),
        "proven empty build debris was not reclaimed"
    );
    assert_eq!(
        r.ok(&["show", forgotten])["items"][0]["availability"],
        "forgotten"
    );
    r.ok(&["rebuild"]);
}

#[test]
fn populated_or_verified_schemaless_generations_are_not_discarded_as_empty_debris() {
    for populated in [false, true] {
        let r = Repo::initialized();
        let note = r.ok(&[
            "note",
            "--text",
            "Synthetic retained payload",
            "--idempotency-key",
            "damaged-archive",
        ]);
        r.ok(&["brief"]);
        let directory = r
            .store_root()
            .join("generations/00000000000000000000000004");
        fs::create_dir(&directory).unwrap();
        let path = directory.join("index.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        if populated {
            db.execute_batch(
                "CREATE TABLE retained_payload(body TEXT); INSERT INTO retained_payload VALUES('Synthetic retained payload');"
            ).unwrap();
        } else {
            db.execute_batch("PRAGMA journal_mode=WAL;").unwrap();
            fs::write(directory.join("VERIFIED"), b"1").unwrap();
        }
        drop(db);
        let before = fs::read(&path).unwrap();
        r.error(&["forget", note["id"].as_str().unwrap()], "purge_failed");
        assert!(directory.exists());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn a_crash_before_schema_creation_is_recovered_with_existing_tombstones() {
    let r = Repo::initialized();
    let note = r.ok(&[
        "note",
        "--text",
        "Synthetic interrupted build",
        "--idempotency-key",
        "interrupted-build",
    ]);
    r.ok(&["forget", note["id"].as_str().unwrap()]);
    let root = r.store_root();
    let before = fs::read_dir(root.join("generations")).unwrap().count();
    let out = r
        .command()
        .env("MEMQ_FAULT", "after_generation_open")
        .arg("rebuild")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(86));
    assert_eq!(
        fs::read_dir(root.join("generations")).unwrap().count(),
        before + 1
    );
    assert!(r.ok(&["brief"])["items"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read_dir(root.join("generations")).unwrap().count(),
        before
    );
    assert_eq!(
        r.ok(&["show", note["id"].as_str().unwrap()])["items"][0]["availability"],
        "forgotten"
    );
}

#[test]
fn legacy_projection_cleanup_preserves_scoped_search_history_and_forget() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"inspection","text":"cideronly local inspection"},
        {"id":"anchor","text":"Surviving orchard evidence"}
    ]));
    r.commit("synthetic local inspection");
    r.git(&["checkout", "-q", "-b", "orchard-side"]);
    r.records(json!([
        {"id":"inspection","text":"pearonly branch inspection"},
        {"id":"anchor","text":"Surviving orchard evidence"}
    ]));
    r.commit("synthetic branch inspection");
    r.git(&["checkout", "-q", "main"]);
    let local = r.ok(&["brief"]);
    let inspection = local["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "inspection")
        .unwrap();
    let id = inspection["id"].as_str().unwrap();
    let anchor = local["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "anchor")
        .unwrap();
    let branch = r.ok(&["brief", "--branches", "orchard-side"]);
    let old_path = r.db_path();
    let db = rusqlite::Connection::open(&old_path).unwrap();
    // Loss of an unused projection must not replace an otherwise usable view.
    db.execute_batch("DROP TABLE IF EXISTS membership; PRAGMA user_version=2;")
        .unwrap();
    drop(db);
    let migrated = r.ok(&["brief", "--branches", "orchard-side"]);
    assert_eq!(
        migrated["freshness"]["view_id"],
        branch["freshness"]["view_id"]
    );
    assert_eq!(r.db_path(), old_path);

    let legacy_projection = "
        CREATE TABLE membership(
            worktree_key TEXT NOT NULL,origin TEXT NOT NULL,branch TEXT NOT NULL,
            item_id TEXT NOT NULL,
            version_id TEXT NOT NULL REFERENCES item_versions(version_id) ON DELETE CASCADE,
            observation_id TEXT NOT NULL REFERENCES observations(observation_id) ON DELETE CASCADE,
            PRIMARY KEY(worktree_key,origin,branch,item_id));
        INSERT OR IGNORE INTO membership
            SELECT json_extract(member_json,'$.observation.worktree_key'),
                   origin,branch,item_id,version_id,observation_id FROM view_items;";
    let db = rusqlite::Connection::open(&old_path).unwrap();
    db.execute_batch(legacy_projection).unwrap();
    drop(db);
    assert_eq!(
        r.ok(&["brief", "--branches", "orchard-side"])["freshness"]["view_id"],
        branch["freshness"]["view_id"]
    );
    let db = rusqlite::Connection::open(&old_path).unwrap();
    assert!(
        db.prepare("SELECT * FROM membership").is_err(),
        "the compatible writable index retained the obsolete projection"
    );
    drop(db);

    r.records(json!([{"id":"anchor","text":"Surviving orchard evidence"}]));
    r.commit("synthetic removal of local inspection");
    r.ok(&["rebuild"]);
    let absent = r.ok(&["brief"]);
    // A retained pre-migration archive can still contain the old projection.
    let db = rusqlite::Connection::open(&old_path).unwrap();
    db.execute_batch(legacy_projection).unwrap();
    db.execute_batch(
        "ALTER TABLE views DROP COLUMN publication_seq;
         ALTER TABLE note_ops DROP COLUMN item_id;
         PRAGMA user_version=2;",
    )
    .unwrap();
    drop(db);
    let historical = r.ok(&[
        "show",
        id,
        "--view-id",
        absent["freshness"]["view_id"].as_str().unwrap(),
    ]);
    assert_eq!(historical["items"][0]["text"], inspection["text"]);
    assert_eq!(historical["items"][0]["availability"], "historical");
    assert!(
        r.ok(&["search", "pearonly"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let branch_args = [
        "search",
        "pearonly",
        "--branches",
        "orchard-side",
        "--view-id",
        branch["freshness"]["view_id"].as_str().unwrap(),
    ];
    let found = r.ok(&branch_args);
    assert_eq!(found["items"].as_array().unwrap().len(), 1);
    assert_eq!(found["items"][0]["id"], id);
    assert_eq!(found["items"][0]["observation"]["branch"], "orchard-side");
    assert!(
        found["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("pearonly branch inspection")
    );
    r.ok(&["forget", id]);
    assert!(r.ok(&branch_args)["items"].as_array().unwrap().is_empty());
    let old_view = local["freshness"]["view_id"].as_str().unwrap();
    assert_eq!(
        r.ok(&["show", id, "--view-id", old_view])["items"][0]["availability"],
        "forgotten"
    );
    assert_eq!(
        r.ok(&[
            "show",
            anchor["id"].as_str().unwrap(),
            "--view-id",
            old_view
        ])["items"][0]["text"],
        anchor["text"]
    );
}

#[test]
fn malformed_capture_cursor_json_falls_back_to_public_source_reads() {
    let r = Repo::initialized();
    let sessions = r.temp.path().join("orchard-sessions");
    fs::create_dir(&sessions).unwrap();
    r.add_source(&format!("[capture]\nomp={}", json!(sessions)));
    let header = json!({
        "type":"session","version":3,"id":"synthetic-orchard-session",
        "cwd":r.root,"branch":"main","timestamp":"2026-09-13T19:00:00Z"
    });
    let event = |id: &str, text: &str| {
        json!({
            "type":"message","id":id,"timestamp":"2026-09-13T19:01:00Z",
            "message":{"role":"assistant","content":[{"type":"text","text":text}]}
        })
    };
    let first = event("entry-one", "First synthetic orchard event");
    let path = sessions.join("orchard.jsonl");
    let mut journal = format!("{header}\n{first}\n");
    fs::write(&path, &journal).unwrap();
    let brief = || -> Value {
        let out = r
            .command()
            .env_remove("MEMQ_OMP_STORE")
            .env_remove("MEMQ_CODEX_STORE")
            .env_remove("MEMQ_OPENCODE_STORE")
            .env_remove("MEMQ_CODE_REPORT")
            .args(["brief", "--budget-kind", "bytes", "--budget", "40000"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        support::budget_check(&out.stdout);
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let before = brief();
    assert_eq!(before["items"].as_array().unwrap().len(), 1);
    let db_path = r.db_path();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    assert_eq!(
        db.execute(
            "UPDATE cursors SET json=?1 WHERE source_id='harness-omp' AND source_path='orchard.jsonl'",
            ["{malformed synthetic cursor"],
        )
        .unwrap(),
        1
    );
    drop(db);
    let unchanged = brief();
    assert_eq!(
        unchanged["freshness"]["view_id"],
        before["freshness"]["view_id"]
    );
    assert_eq!(unchanged["items"], before["items"]);

    journal.push_str(&format!(
        "{}\n",
        event("entry-two", "Appended synthetic orchard event")
    ));
    fs::write(&path, journal).unwrap();
    let after = brief();
    assert_eq!(after["freshness"]["status"], "current");
    assert_eq!(after["coverage"]["capture_incomplete"], false);
    let items = after["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let original = items
        .iter()
        .find(|item| item["id"] == before["items"][0]["id"])
        .unwrap();
    assert_eq!(original["text"], before["items"][0]["text"]);
    assert_eq!(original["pointer"], before["items"][0]["pointer"]);
    assert!(items.iter().any(|item| {
        item["text"]
            .as_str()
            .unwrap()
            .contains("Appended synthetic orchard event")
    }));
    assert_eq!(r.db_path(), db_path);
    let db = rusqlite::Connection::open(db_path).unwrap();
    let cursor: String = db
        .query_row(
            "SELECT json FROM cursors WHERE source_id='harness-omp' AND source_path='orchard.jsonl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(serde_json::from_str::<Value>(&cursor).is_ok());
}
