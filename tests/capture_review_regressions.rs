mod support;

use rusqlite::{Connection, types::ValueRef};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use support::Repo;

fn run(repo: &Repo, args: &[&str]) -> Value {
    let output = repo
        .command()
        .env_remove("MEMQ_OMP_STORE")
        .env_remove("MEMQ_CODEX_STORE")
        .env_remove("MEMQ_OPENCODE_STORE")
        .env_remove("MEMQ_CODE_REPORT")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "CLI failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn brief(repo: &Repo) -> Value {
    run(
        repo,
        &["brief", "--budget-kind", "bytes", "--budget", "60000"],
    )
}

fn configure(repo: &Repo, kind: &str, path: &Path) {
    repo.add_source(&format!("[capture]\n{kind}={}", json!(path)));
}

fn capture_error(response: &Value, kind: &str, reason: &str) {
    assert_eq!(response["coverage"]["capture_incomplete"], true);
    let coverage = &response["coverage"]["capture"][format!("harness-{kind}")];
    assert_eq!(coverage["status"], "incomplete");
    assert!(
        coverage["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["reason"] == reason),
        "missing explicit capture diagnostic"
    );
}

fn journal(repo: &Repo, kind: &str, session: &str, records: &[(&str, &str)]) -> String {
    let header = match kind {
        "omp" => json!({
            "type":"session", "version":3, "id":session, "cwd":repo.root,
            "timestamp":"2026-09-13T19:00:00Z"
        }),
        "codex" => json!({
            "type":"session_meta", "ordinal":0,
            "payload":{"id":session, "cwd":repo.root, "cli_version":"0.154.0",
                "git":{"branch":"main"}}
        }),
        _ => unreachable!(),
    };
    let mut text = format!("{header}\n");
    for (entry, body) in records {
        let record = match kind {
            "omp" => json!({
                "type":"message", "id":entry, "parentId":null,
                "timestamp":"2026-09-13T19:01:00Z",
                "message":{"role":"assistant", "content":[{"type":"text", "text":body}]}
            }),
            "codex" => json!({
                "type":"response_item", "ordinal":entry.parse::<u64>().unwrap(),
                "timestamp":"2026-09-13T19:01:00Z",
                "payload":{"type":"message", "role":"assistant",
                    "content":[{"type":"output_text", "text":body}]}
            }),
            _ => unreachable!(),
        };
        text.push_str(&format!("{record}\n"));
    }
    text
}

fn assert_no_persisted_secret(repo: &Repo, secret: &str) {
    let db = Connection::open(repo.db_path()).unwrap();
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for table in tables {
        let mut statement = db
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for column in 0..columns {
                if let ValueRef::Text(value) = row.get_ref(column).unwrap() {
                    assert!(
                        !String::from_utf8_lossy(value).contains(secret),
                        "sensitive fixture value persisted in {table}"
                    );
                }
            }
        }
    }
}

fn cursors(repo: &Repo) -> Vec<(String, Value)> {
    let db = Connection::open(repo.db_path()).unwrap();
    db.prepare("SELECT source_path,json FROM cursors ORDER BY source_path")
        .unwrap()
        .query_map([], |row| {
            let cursor: String = row.get(1)?;
            Ok((row.get(0)?, serde_json::from_str(&cursor).unwrap()))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn copied_journal(kind: &str) {
    let repo = Repo::initialized();
    let root = repo.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("01-original.jsonl"),
        journal(&repo, kind, "synthetic-copy", &[("1", "Original evidence")]),
    )
    .unwrap();
    configure(&repo, kind, &root);
    let first = brief(&repo);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    fs::copy(root.join("01-original.jsonl"), root.join("02-copy.jsonl")).unwrap();
    let copied = brief(&repo);
    assert_eq!(copied["items"].as_array().unwrap().len(), 1);
    assert_eq!(copied["items"][0]["id"], first["items"][0]["id"]);
    assert_eq!(
        copied["items"][0]["version_id"],
        first["items"][0]["version_id"]
    );
    assert_eq!(copied["coverage"]["capture_incomplete"], false);
    assert_eq!(
        copied["coverage"]["capture"][format!("harness-{kind}")]["accepted"],
        1
    );
    assert_eq!(cursors(&repo).len(), 2);
    let repeated = brief(&repo);
    assert_eq!(
        copied["freshness"]["view_id"],
        repeated["freshness"]["view_id"]
    );
}

#[test]
fn omp_copied_session_coalesces() {
    copied_journal("omp");
}

#[test]
fn codex_copied_session_coalesces() {
    copied_journal("codex");
}

fn overlapping_journals(kind: &str) {
    let repo = Repo::initialized();
    let root = repo.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    let original = journal(
        &repo,
        kind,
        "synthetic-overlap",
        &[("1", "First evidence"), ("2", "Shared evidence")],
    );
    let active = journal(
        &repo,
        kind,
        "synthetic-overlap",
        &[("2", "Shared evidence"), ("3", "Latest evidence")],
    );
    fs::write(root.join("01-original.jsonl"), &original).unwrap();
    configure(&repo, kind, &root);
    let first = brief(&repo);
    fs::write(root.join("02-active.jsonl"), &active).unwrap();
    let overlap = brief(&repo);
    assert_eq!(overlap["items"].as_array().unwrap().len(), 3);
    assert_eq!(overlap["coverage"]["capture_incomplete"], false);
    for item in first["items"].as_array().unwrap() {
        let current = overlap["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|current| current["id"] == item["id"])
            .unwrap();
        assert_eq!(current["version_id"], item["version_id"]);
        assert_eq!(current["pointer"], item["pointer"]);
    }
    for (path, cursor) in cursors(&repo) {
        assert_eq!(
            cursor["byte_offset"],
            fs::metadata(root.join(path)).unwrap().len()
        );
        assert_eq!(cursor["complete"], true);
    }
    fs::rename(
        root.join("01-original.jsonl"),
        root.join("03-rotated.jsonl"),
    )
    .unwrap();
    let rotated = brief(&repo);
    assert_eq!(rotated["items"].as_array().unwrap().len(), 3);
    for item in rotated["items"].as_array().unwrap() {
        let previous = overlap["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|previous| previous["id"] == item["id"])
            .unwrap();
        assert_eq!(item["version_id"], previous["version_id"]);
        let source = &item["observation"];
        let bytes = fs::read(root.join(source["source_path"].as_str().unwrap())).unwrap();
        let begin = source["byte_range"][0].as_u64().unwrap() as usize;
        let end = source["byte_range"][1].as_u64().unwrap() as usize;
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes[begin..end]).unwrap(),
            serde_json::from_str::<Value>(item["text"].as_str().unwrap()).unwrap()
        );
    }
    let repeated = brief(&repo);
    assert_eq!(
        rotated["freshness"]["view_id"],
        repeated["freshness"]["view_id"]
    );
    let old = run(
        &repo,
        &[
            "show",
            first["items"][0]["id"].as_str().unwrap(),
            "--view-id",
            first["freshness"]["view_id"].as_str().unwrap(),
            "--budget-kind",
            "bytes",
            "--budget",
            "60000",
        ],
    );
    assert_eq!(old["items"][0]["pointer"], first["items"][0]["pointer"]);
    assert_eq!(
        old["items"][0]["observation"],
        first["items"][0]["observation"]
    );
}

#[test]
fn omp_rotated_overlapping_files_keep_identity_and_exact_observations() {
    overlapping_journals("omp");
}

#[test]
fn codex_rotated_overlapping_files_keep_identity_and_exact_observations() {
    overlapping_journals("codex");
}

fn conflicting_journals(kind: &str, partial: bool) {
    let repo = Repo::initialized();
    let root = repo.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    let original = journal(
        &repo,
        kind,
        "synthetic-conflict",
        &[("1", "Original evidence")],
    );
    fs::write(root.join("01-original.jsonl"), &original).unwrap();
    if partial {
        fs::write(
            root.join("04-independent.jsonl"),
            journal(
                &repo,
                kind,
                "synthetic-independent",
                &[("1", "Independent evidence")],
            ),
        )
        .unwrap();
    }
    configure(&repo, kind, &root);
    let first = brief(&repo);
    fs::write(
        root.join("02-conflict.jsonl"),
        journal(
            &repo,
            kind,
            "synthetic-conflict",
            &[("1", "Conflicting evidence")],
        ),
    )
    .unwrap();
    fs::write(root.join("03-copy.jsonl"), &original).unwrap();
    let conflict = brief(&repo);
    capture_error(&conflict, kind, "capture_conflict");
    assert_ne!(conflict["freshness"]["status"], "current");
    assert!(!conflict.to_string().contains("Conflicting evidence"));
    if partial {
        assert_eq!(conflict["items"].as_array().unwrap().len(), 1);
        assert!(
            conflict["items"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Independent evidence")
        );
        for (path, cursor) in cursors(&repo) {
            if path == "04-independent.jsonl" {
                assert_eq!(cursor["complete"], true);
            } else {
                assert_eq!(cursor["complete"], false);
                assert_eq!(cursor["byte_offset"], 0);
                assert_eq!(cursor["last_id_or_ordinal"], Value::Null);
            }
        }
    } else {
        assert_eq!(conflict["items"], first["items"]);
        assert_eq!(conflict["freshness"]["status"], "stale");
        assert_eq!(
            conflict["freshness"]["view_id"],
            first["freshness"]["view_id"]
        );
    }
    let index = Connection::open(repo.db_path()).unwrap();
    assert_eq!(
        index
            .query_row(
                "SELECT count(*) FROM item_versions WHERE redacted_text LIKE '%Conflicting evidence%'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    drop(index);
    let compact = run(
        &repo,
        &[
            "brief",
            "--compact",
            "--budget-kind",
            "bytes",
            "--budget",
            "60000",
        ],
    );
    capture_error(&compact, kind, "capture_conflict");
    let expanded = support::expanded_items(&compact);
    assert_eq!(expanded.len(), conflict["items"].as_array().unwrap().len());
    for item in expanded {
        let full = conflict["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|full| full["id"] == item["id"])
            .unwrap();
        assert_eq!(item["pointer"], full["pointer"]);
        assert_eq!(item["text"], full["text"]);
    }
    fs::write(root.join("02-conflict.jsonl"), original).unwrap();
    let repaired = brief(&repo);
    assert_eq!(repaired["coverage"]["capture_incomplete"], false);
    assert_eq!(
        repaired["items"].as_array().unwrap().len(),
        if partial { 2 } else { 1 }
    );
    for item in first["items"].as_array().unwrap() {
        assert!(repaired["items"].as_array().unwrap().iter().any(|current| {
            current["id"] == item["id"] && current["version_id"] == item["version_id"]
        }));
    }
    assert!(
        cursors(&repo)
            .iter()
            .all(|(_, cursor)| cursor["complete"] == true)
    );
}

#[test]
fn omp_conflicts_preserve_the_last_usable_view_and_retry() {
    conflicting_journals("omp", false);
}

#[test]
fn codex_conflicts_preserve_the_last_usable_view_and_retry() {
    conflicting_journals("codex", false);
}

#[test]
fn omp_conflicts_pause_only_ambiguous_records() {
    conflicting_journals("omp", true);
}

#[test]
fn codex_conflicts_pause_only_ambiguous_records() {
    conflicting_journals("codex", true);
}

fn unsafe_journal_identity(kind: &str, session_identity: bool) {
    let repo = Repo::initialized();
    let root = repo.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    let secrets = ["x", "y"].map(|c| format!("sk-{}", c.repeat(24)));
    for (index, secret) in secrets.iter().enumerate() {
        let (session, entry) = if session_identity {
            (secret.as_str(), "1")
        } else {
            ("synthetic-sensitive", secret.as_str())
        };
        fs::write(
            root.join(format!("session-{index}.jsonl")),
            journal(&repo, kind, session, &[(entry, secret)]),
        )
        .unwrap();
    }
    configure(&repo, kind, &root);
    let result = brief(&repo);
    for secret in &secrets {
        assert_no_persisted_secret(&repo, secret);
        assert!(!result.to_string().contains(secret));
    }
    capture_error(&result, kind, "capture_unsafe_metadata");
    assert!(result["items"].as_array().unwrap().is_empty());
    for (index, secret) in secrets.iter().enumerate() {
        fs::write(
            root.join(format!("session-{index}.jsonl")),
            journal(
                &repo,
                kind,
                &format!("synthetic-repaired-{index}"),
                &[("1", secret)],
            ),
        )
        .unwrap();
    }
    let repaired = brief(&repo);
    assert_eq!(repaired["items"].as_array().unwrap().len(), 2);
    assert_eq!(repaired["coverage"]["capture_incomplete"], false);
    let ids: BTreeSet<_> = repaired["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    for secret in &secrets {
        assert_no_persisted_secret(&repo, secret);
        assert!(!repaired.to_string().contains(secret));
    }
}

#[test]
fn omp_sensitive_entry_identity_is_rejected() {
    unsafe_journal_identity("omp", false);
}

#[test]
fn omp_sensitive_session_identity_is_rejected() {
    unsafe_journal_identity("omp", true);
}

#[test]
fn codex_sensitive_session_identity_is_rejected() {
    unsafe_journal_identity("codex", true);
}

fn unsafe_opencode_identity(field: &str) {
    let repo = Repo::initialized();
    let path = repo.temp.path().join("opencode.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/harness/opencode/build.sql"))
        .unwrap();
    let secret = format!("sk-{}", "x".repeat(24));
    let (session, part, message) = match field {
        "session" => (secret.as_str(), "synthetic-part", "synthetic-message"),
        "part" => ("synthetic-session", secret.as_str(), "synthetic-message"),
        "message" => ("synthetic-session", "synthetic-part", secret.as_str()),
        _ => unreachable!(),
    };
    db.execute(
        "INSERT INTO session VALUES(?1,?2,'1.18.30',1000)",
        rusqlite::params![session, repo.root.to_string_lossy()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES(?1,?2,1000,?3)",
        rusqlite::params![message, session, r#"{"role":"assistant"}"#],
    )
    .unwrap();
    db.execute(
        "INSERT INTO part VALUES(?1,?2,?3,1000,1000,?4)",
        rusqlite::params![
            part,
            session,
            message,
            json!({"type":"text", "text":secret}).to_string()
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO session VALUES('synthetic-safe',?1,'1.18.30',1000)",
        [repo.root.to_string_lossy()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES('safe-message','synthetic-safe',1000,?1)",
        [r#"{"role":"assistant"}"#],
    )
    .unwrap();
    db.execute(
        "INSERT INTO part VALUES('safe-part','synthetic-safe','safe-message',1000,1000,?1)",
        [r#"{"type":"text","text":"Independent evidence"}"#],
    )
    .unwrap();
    drop(db);
    configure(&repo, "opencode", &path);
    let result = brief(&repo);
    assert_no_persisted_secret(&repo, &secret);
    assert!(!result.to_string().contains(&secret));
    capture_error(&result, "opencode", "capture_unsafe_metadata");
    assert_eq!(result["items"].as_array().unwrap().len(), 1);
    assert!(
        result["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Independent evidence")
    );
    assert_eq!(cursors(&repo)[0].1["complete"], false);
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE session SET id='repaired-session' WHERE id=?1",
        [session],
    )
    .unwrap();
    db.execute(
        "UPDATE message SET id='repaired-message',session_id='repaired-session' WHERE id=?1",
        [message],
    )
    .unwrap();
    db.execute(
        "UPDATE part SET id='repaired-part',session_id='repaired-session',message_id='repaired-message' WHERE id=?1",
        [part],
    ).unwrap();
    drop(db);
    let repaired = brief(&repo);
    assert_eq!(repaired["items"].as_array().unwrap().len(), 2);
    assert_eq!(repaired["coverage"]["capture_incomplete"], false);
    assert_eq!(cursors(&repo)[0].1["complete"], true);
    assert!(!repaired.to_string().contains(&secret));
    assert_no_persisted_secret(&repo, &secret);
}

#[test]
fn opencode_sensitive_session_identity_is_rejected() {
    unsafe_opencode_identity("session");
}

#[test]
fn opencode_sensitive_part_identity_is_rejected() {
    unsafe_opencode_identity("part");
}

#[test]
fn opencode_sensitive_message_identity_is_rejected() {
    unsafe_opencode_identity("message");
}

fn unsafe_jsonl_paths(kind: &str) {
    for component in ["file", "directory", "root", "missing"] {
        let repo = Repo::initialized();
        let secret = format!("sk-{}", "x".repeat(24));
        let root = repo.temp.path().join(if component == "root" {
            secret.as_str()
        } else {
            "sessions"
        });
        let relative = match component {
            "file" | "missing" => format!("{secret}.jsonl"),
            "directory" => format!("{secret}/session.jsonl"),
            _ => "session.jsonl".into(),
        };
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        if component != "missing" {
            fs::write(
                &path,
                journal(&repo, kind, "synthetic-path", &[("1", "Path evidence")]),
            )
            .unwrap();
        }
        let configured = if component == "missing" { &path } else { &root };
        configure(&repo, kind, configured);
        let result = brief(&repo);
        assert_no_persisted_secret(&repo, &secret);
        assert!(!result.to_string().contains(&secret));
        capture_error(&result, kind, "capture_unsafe_metadata");
        assert!(result["items"].as_array().unwrap().is_empty());
    }
}

#[test]
fn omp_sensitive_source_path_components_are_rejected() {
    unsafe_jsonl_paths("omp");
}

#[test]
fn codex_sensitive_source_path_components_are_rejected() {
    unsafe_jsonl_paths("codex");
}

#[test]
fn opencode_sensitive_source_path_components_are_rejected() {
    for component in ["file", "directory", "missing"] {
        let repo = Repo::initialized();
        let secret = format!("sk-{}", "x".repeat(24));
        let root = repo.temp.path().join(if component == "directory" {
            secret.as_str()
        } else {
            "database"
        });
        fs::create_dir_all(&root).unwrap();
        let path = root.join(if component == "file" || component == "missing" {
            format!("{secret}.db")
        } else {
            "opencode.db".into()
        });
        if component != "missing" {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(include_str!("fixtures/harness/opencode/build.sql"))
                .unwrap();
        }
        configure(&repo, "opencode", &path);
        let result = brief(&repo);
        assert_no_persisted_secret(&repo, &secret);
        assert!(!result.to_string().contains(&secret));
        capture_error(&result, "opencode", "capture_unsafe_metadata");
    }
}

fn unsafe_entry_cursor(kind: &str, credential_pin: bool) {
    let repo = Repo::initialized();
    let root = repo.temp.path().join("sessions");
    fs::create_dir(&root).unwrap();
    let secret = format!("sk-{}", "x".repeat(24));
    let prefix = journal(&repo, kind, "synthetic-cursor", &[("1", "Safe prefix")]);
    let next = journal(&repo, kind, "synthetic-cursor", &[("2", &secret)]);
    let mut rejected: Value = serde_json::from_str(next.lines().nth(1).unwrap()).unwrap();
    rejected[if kind == "omp" { "id" } else { "ordinal" }] = json!(secret);
    if credential_pin {
        rejected["type"] = json!("credential_pin");
    }
    let tail = journal(&repo, kind, "synthetic-cursor", &[("3", "Safe tail")]);
    let tail = tail.lines().nth(1).unwrap();
    let path = root.join("session.jsonl");
    fs::write(&path, format!("{prefix}{rejected}\n{tail}\n")).unwrap();
    configure(&repo, kind, &root);
    let incomplete = brief(&repo);
    capture_error(
        &incomplete,
        kind,
        if kind == "omp" {
            "capture_unsafe_metadata"
        } else {
            "capture_schema"
        },
    );
    assert_eq!(incomplete["items"].as_array().unwrap().len(), 1);
    assert_eq!(cursors(&repo)[0].1["byte_offset"], prefix.len());
    assert_eq!(cursors(&repo)[0].1["last_id_or_ordinal"], "1");
    assert_eq!(cursors(&repo)[0].1["complete"], false);
    assert!(!incomplete.to_string().contains(&secret));
    assert_no_persisted_secret(&repo, &secret);
    fs::write(
        &path,
        format!("{prefix}{}\n{tail}\n", next.lines().nth(1).unwrap()),
    )
    .unwrap();
    let repaired = brief(&repo);
    assert_eq!(repaired["items"].as_array().unwrap().len(), 3);
    assert_eq!(repaired["coverage"]["capture_incomplete"], false);
    assert_eq!(
        cursors(&repo)[0].1["byte_offset"],
        fs::metadata(path).unwrap().len()
    );
    assert_eq!(cursors(&repo)[0].1["last_id_or_ordinal"], "3");
    assert_eq!(cursors(&repo)[0].1["complete"], true);
    assert!(repaired["items"].as_array().unwrap().iter().any(|item| {
        item["id"] == incomplete["items"][0]["id"]
            && item["version_id"] == incomplete["items"][0]["version_id"]
    }));
    assert_no_persisted_secret(&repo, &secret);
}

#[test]
fn omp_unsafe_entry_stops_cursor_and_replays_after_repair() {
    unsafe_entry_cursor("omp", false);
}

#[test]
fn omp_unsafe_excluded_identity_cannot_enter_cursor_metadata() {
    unsafe_entry_cursor("omp", true);
}

#[test]
fn codex_sensitive_ordinal_is_not_an_invented_identity() {
    unsafe_entry_cursor("codex", false);
}

#[test]
fn matching_content_with_conflicting_session_scope_is_paused() {
    for kind in ["omp", "codex"] {
        let repo = Repo::initialized();
        let root = repo.temp.path().join("sessions");
        fs::create_dir(&root).unwrap();
        let original = journal(&repo, kind, "synthetic-scope", &[("1", "Scoped evidence")]);
        fs::write(root.join("original.jsonl"), &original).unwrap();
        configure(&repo, kind, &root);
        let first = brief(&repo);
        let mut header: Value = serde_json::from_str(original.lines().next().unwrap()).unwrap();
        if kind == "omp" {
            header["branch"] = json!("other-branch");
        } else {
            header["payload"]["git"]["branch"] = json!("other-branch");
        }
        fs::write(
            root.join("copy.jsonl"),
            format!("{header}\n{}\n", original.lines().nth(1).unwrap()),
        )
        .unwrap();
        let result = brief(&repo);
        capture_error(&result, kind, "capture_conflict");
        assert_eq!(result["freshness"]["status"], "stale");
        assert_eq!(result["items"], first["items"]);
    }
}

fn mixed_opencode_versions(
    unsupported_index: usize,
    unsupported_version: &str,
    expected_version: &str,
) {
    let repo = Repo::initialized();
    let path = repo.temp.path().join("opencode.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/harness/opencode/build.sql"))
        .unwrap();
    let sessions = ["synthetic-a", "synthetic-b", "synthetic-c"];
    for (index, session) in sessions.iter().enumerate() {
        let version = if index == unsupported_index {
            unsupported_version
        } else {
            "1.18.30"
        };
        db.execute(
            "INSERT INTO session VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                session,
                repo.root.to_string_lossy(),
                version,
                if index == unsupported_index {
                    9000
                } else {
                    1000
                }
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES(?1,?2,1000,?3)",
            rusqlite::params![
                format!("message-{index}"),
                session,
                r#"{"role":"assistant"}"#
            ],
        )
        .unwrap();
        // The unknown version may have different record semantics. Do not
        // interpret its payload merely because the database columns match.
        let data = if index == unsupported_index {
            "unsupported part encoding".to_owned()
        } else {
            json!({"type":"text","text":format!("Supported evidence {index}")}).to_string()
        };
        db.execute(
            "INSERT INTO part VALUES(?1,?2,?3,1000,1000,?4)",
            rusqlite::params![
                format!("part-{index}"),
                session,
                format!("message-{index}"),
                data
            ],
        )
        .unwrap();
    }
    drop(db);
    configure(&repo, "opencode", &path);
    let captured = run(&repo, &["capture"]);
    assert_eq!(
        captured["coverage"]["capture"]["harness-opencode"]["accepted"], 2,
        "a version-paused session must not suppress later supported sessions"
    );
    capture_error(&captured, "opencode", "capture_unsupported_version");
    let errors = captured["coverage"]["capture"]["harness-opencode"]["errors"]
        .as_array()
        .unwrap();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["session_id"], sessions[unsupported_index]);
    assert_eq!(errors[0]["version"], expected_version);
    assert_eq!(errors[0]["status"], "paused");
    if unsupported_version != expected_version {
        assert!(!captured.to_string().contains(unsupported_version));
        assert_no_persisted_secret(&repo, unsupported_version);
    }
    let first = brief(&repo);
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(first["freshness"]["status"], "incomplete");
    for (index, session) in sessions.iter().enumerate() {
        let native = format!("{session}/part%3Apart-{index}");
        assert_eq!(
            first["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"].as_str().unwrap().ends_with(&native)),
            index != unsupported_index
        );
    }
    let cursor = &cursors(&repo)[0].1;
    assert_eq!(cursor["complete"], false);
    assert_eq!(cursor["last_time_updated"], 1000);
    assert_eq!(cursor["seen_ids_window"].as_array().unwrap().len(), 2);
    assert_eq!(
        first["freshness"]["view_id"],
        brief(&repo)["freshness"]["view_id"]
    );
    let compact = run(
        &repo,
        &[
            "brief",
            "--compact",
            "--budget-kind",
            "bytes",
            "--budget",
            "60000",
        ],
    );
    capture_error(&compact, "opencode", "capture_unsupported_version");
    assert_eq!(support::expanded_items(&compact).len(), 2);
    let db = Connection::open(&path).unwrap();
    db.execute(
        "UPDATE session SET version='1.18.30',time_updated=10 WHERE id=?1",
        [sessions[unsupported_index]],
    )
    .unwrap();
    db.execute(
        "UPDATE part SET data=?1 WHERE session_id=?2",
        rusqlite::params![
            r#"{"type":"text","text":"Newly supported evidence"}"#,
            sessions[unsupported_index]
        ],
    )
    .unwrap();
    drop(db);
    let repaired = brief(&repo);
    assert_eq!(repaired["coverage"]["capture_incomplete"], false);
    assert_eq!(repaired["items"].as_array().unwrap().len(), 3);
    assert_eq!(cursors(&repo)[0].1["complete"], true);
    for item in first["items"].as_array().unwrap() {
        assert!(repaired["items"].as_array().unwrap().iter().any(|current| {
            current["id"] == item["id"] && current["version_id"] == item["version_id"]
        }));
    }
}

#[test]
fn opencode_unsupported_version_first_does_not_block_supported_sessions() {
    mixed_opencode_versions(0, "1.17.9", "1.17.9");
}

#[test]
fn opencode_unsupported_version_between_supported_sessions_is_paused() {
    mixed_opencode_versions(1, "1.19.0", "1.19.0");
}

#[test]
fn opencode_unsupported_version_last_is_reported_individually() {
    mixed_opencode_versions(2, "1.19.0", "1.19.0");
}

#[test]
fn opencode_unsupported_version_diagnostic_redacts_sensitive_values() {
    mixed_opencode_versions(1, &format!("sk-{}", "x".repeat(24)), "[REDACTED]");
}
