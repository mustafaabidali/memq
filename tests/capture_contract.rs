mod support;
use serde_json::{Value, json};
use std::fs;
use support::Repo;

fn jsonl(r: &Repo, kind: &str, session: &str, entry: &str, content: &str) -> String {
    let header = if kind == "omp" {
        json!({"type":"session","version":3,"id":session,"cwd":r.root,"timestamp":"2026-09-13T19:00:00Z"})
    } else {
        json!({"type":"session_meta","ordinal":0,"payload":{"id":session,"cwd":r.root,"cli_version":"0.154.0","git":{"branch":"main"}}})
    };
    let record = if kind == "omp" {
        json!({"type":"message","id":entry,"parentId":null,"timestamp":"2026-09-13T19:01:00Z","message":{"role":"assistant","content":[{"type":"text","text":content}]}})
    } else {
        json!({"type":"response_item","ordinal":1,"timestamp":"2026-09-13T19:01:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":content}]}})
    };
    format!("{header}\n{record}\n")
}

fn configure(r: &Repo, kind: &str, root: &std::path::Path) {
    let text = fs::read_to_string(r.root.join(".memq/config.toml")).unwrap();
    r.write(
        ".memq/config.toml",
        format!("{text}\n[capture]\n{kind}={}\n", json!(root)),
    );
}

#[test]
fn session_qualified_ids_replay_and_incomplete_tail() {
    for kind in ["omp", "codex"] {
        let r = Repo::initialized();
        let root = r.temp.path().join("sessions");
        fs::create_dir(&root).unwrap();
        for s in ["synthetic-a", "synthetic-b"] {
            fs::write(
                root.join(format!("{s}.jsonl")),
                jsonl(&r, kind, s, "one", "Captured evidence"),
            )
            .unwrap();
        }
        configure(&r, kind, &root);
        r.ok(&["capture"]);
        let db = rusqlite::Connection::open(r.db_path()).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM items WHERE source_id=?1",
                [format!("harness-{kind}")],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        let before: i64 = db
            .query_row("SELECT count(*) FROM item_versions", [], |r| r.get(0))
            .unwrap();
        let first = r.ok(&["brief"]);
        let again = r.ok(&["brief"]);
        assert_eq!(first["freshness"]["view_id"], again["freshness"]["view_id"]);
        assert_eq!(
            db.query_row("SELECT count(*) FROM item_versions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            before
        );
        let path = root.join("synthetic-a.jsonl");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{original}{{\"type\":")).unwrap();
        let b = r.ok(&["brief"]);
        assert_eq!(b["coverage"]["capture_incomplete"], true);
        let cursor: String = db
            .query_row(
                "SELECT json FROM cursors WHERE source_path='synthetic-a.jsonl'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let c: Value = serde_json::from_str(&cursor).unwrap();
        assert_eq!(c["byte_offset"], original.len() as u64);
    }
}

#[test]
fn opencode_mutable_parts_use_full_inventory_not_timestamp_window() {
    let r = Repo::initialized();
    let path = r.temp.path().join("opencode.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/harness/opencode/build.sql"))
        .unwrap();
    for s in ["synthetic-a", "synthetic-b"] {
        db.execute(
            "INSERT INTO session VALUES(?1,?2,'1.18.30',1000)",
            rusqlite::params![s, r.root.to_string_lossy()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES(?1,?2,1000,?3)",
            rusqlite::params![format!("m-{s}"), s, r#"{"role":"assistant"}"#],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES(?1,?2,?3,1000,1000,?4)",
            rusqlite::params![
                format!("p-{s}"),
                s,
                format!("m-{s}"),
                r#"{"type":"text","text":"before"}"#
            ],
        )
        .unwrap();
    }
    configure(&r, "opencode", &path);
    r.ok(&["capture"]);
    db.execute(
        "UPDATE part SET data=?1,time_updated=10 WHERE id='p-synthetic-a'",
        [r#"{"type":"text","text":"after rollback"}"#],
    )
    .unwrap();
    r.ok(&["capture"]);
    let index = rusqlite::Connection::open(r.db_path()).unwrap();
    assert_eq!(
        index
            .query_row(
                "SELECT count(*) FROM item_versions WHERE redacted_text LIKE '%after rollback%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        index
            .query_row("SELECT count(*) FROM item_versions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        3
    );
    db.execute_batch("ALTER TABLE part RENAME COLUMN time_updated TO changed_at")
        .unwrap();
    let result = r.ok(&["capture"]);
    assert_eq!(result["coverage"]["capture_incomplete"], true);
}

#[test]
fn nested_repositories_rejected_and_linked_worktree_accepted() {
    let r = Repo::initialized();
    r.commit("config");
    let linked = r.linked("side");
    let nested = r.root.join("nested");
    fs::create_dir(&nested).unwrap();
    support::git(&nested, &["init", "-q"]);
    let root = r.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    for (id, cwd) in [
        ("linked", &linked),
        ("nested", &nested),
        ("gone", &r.temp.path().join("gone")),
    ] {
        let mut header = jsonl(&r, "omp", id, "one", "scope");
        header = header.replace(&json!(r.root).to_string(), &json!(cwd).to_string());
        fs::write(root.join(format!("{id}.jsonl")), header).unwrap();
    }
    configure(&r, "omp", &root);
    let result = r.ok(&["capture"]);
    assert_eq!(result["coverage"]["capture_incomplete"], true);
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM items WHERE source_id='harness-omp'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM observations WHERE worktree_key=?1",
            [std::fs::canonicalize(&linked)
                .unwrap()
                .to_string_lossy()
                .as_ref()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[test]
fn oversized_multibyte_evidence_is_not_silently_cut() {
    let r = Repo::initialized();
    let root = r.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    let text = "evidence العربية 0123456789\n".repeat(400_000);
    assert!(text.len() > 12 * 1024 * 1024);
    let line = jsonl(&r, "codex", "synthetic-large", "one", &text);
    fs::write(root.join("large.jsonl"), &line).unwrap();
    configure(&r, "codex", &root);
    r.ok(&["capture"]);
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let captured: String = db
        .query_row("SELECT redacted_text FROM item_versions", [], |r| r.get(0))
        .unwrap();
    let original: Value = serde_json::from_str(line.lines().nth(1).unwrap()).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&captured).unwrap(), original);
    let cursor: String = db
        .query_row("SELECT json FROM cursors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&cursor).unwrap()["byte_offset"],
        line.len() as u64
    );
}
