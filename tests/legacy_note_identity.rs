mod support;

use serde_json::{Value, json};
use std::fs;
use std::process::Command;
use support::Repo;

const BODY: &str = "Recover the synthetic orchard inspection";
const KEY: &str = "orchard-legacy-step";
const NOTE: &[&str] = &["note", "--text", BODY, "--idempotency-key", KEY];

fn command(repo: &Repo) -> Command {
    // The same public regression can exercise a frozen review binary without
    // rebuilding it or embedding a machine-specific path in the test.
    let binary = std::env::var_os("MEMQ_LEGACY_NOTE_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_memq").into());
    let mut command = Command::new(binary);
    command
        .arg("--repo")
        .arg(&repo.root)
        .env("MEMQ_DATA_DIR", &repo.data)
        .env("MEMQ_NOW", "2026-09-13T20:00:00Z")
        .env_remove("MEMQ_OMP_STORE")
        .env_remove("MEMQ_CODEX_STORE")
        .env_remove("MEMQ_OPENCODE_STORE")
        .env_remove("MEMQ_CODE_REPORT");
    command
}

fn call(repo: &Repo, args: &[&str]) -> Value {
    let output = command(repo).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    if response.get("budget").is_some() {
        support::budget_check(&output.stdout);
    }
    response
}

fn interrupted_legacy_note() -> (Repo, Value, Vec<u8>) {
    let repo = Repo::new();
    call(&repo, &["init"]);
    repo.record_source();
    repo.records(json!([{"id":"anchor","text":"Surviving synthetic orchard evidence"}]));
    let original = call(&repo, NOTE);
    let path = repo.root.join(original["path"].as_str().unwrap());
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    let db = rusqlite::Connection::open(repo.db_path()).unwrap();
    db.execute_batch(
        "UPDATE note_ops SET state='writing';
         ALTER TABLE note_ops DROP COLUMN item_id;
         PRAGMA user_version=2;",
    )
    .unwrap();
    drop(db);
    (repo, original, bytes)
}

fn prepend_notes_source(repo: &Repo, path: &str) -> String {
    let config = repo.root.join(".memq/config.toml");
    let previous = fs::read_to_string(&config).unwrap();
    let position = previous.find("[[source]]").unwrap();
    let mut changed = previous.clone();
    changed.insert_str(
        position,
        &format!(
            "[[source]]\nid = \"orchard-extra\"\nkind = \"memq-notes\"\npath = \"{path}\"\n\n"
        ),
    );
    fs::write(config, changed).unwrap();
    previous
}

fn assert_original_note(repo: &Repo, original: &Value, retry: &Value, bytes: &[u8]) {
    assert_eq!(retry["id"], original["id"]);
    assert_eq!(retry["note_id"], original["note_id"]);
    assert_eq!(retry["path"], original["path"]);
    assert_eq!(
        fs::read(repo.root.join(original["path"].as_str().unwrap())).unwrap(),
        bytes
    );
    assert_eq!(
        fs::read_dir(repo.root.join(".memq/notes")).unwrap().count(),
        1
    );
    assert!(!repo.root.join(".memq/extra-notes").exists());
    let shown = call(repo, &["show", retry["id"].as_str().unwrap()]);
    assert!(
        shown["items"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains(BODY)),
        "the first returned identity must be readable: {shown}"
    );
}

#[test]
fn first_legacy_retry_returns_the_original_readable_identity() {
    let (repo, original, bytes) = interrupted_legacy_note();
    prepend_notes_source(&repo, ".memq/extra-notes");
    let retry = call(&repo, NOTE);
    let visible = call(
        &repo,
        &["brief", "--budget-kind", "bytes", "--budget", "40000"],
    );
    let first_read = call(&repo, &["show", retry["id"].as_str().unwrap()]);
    let second = call(&repo, NOTE);
    assert_eq!(
        retry["id"], original["id"],
        "first retry availability: {}; second retry identity: {}",
        first_read["items"][0]["availability"], second["id"]
    );
    assert!(
        visible["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == retry["id"])
    );
    assert_original_note(&repo, &original, &retry, &bytes);
    assert_original_note(&repo, &original, &second, &bytes);
    call(&repo, &["rebuild"]);
    assert_original_note(&repo, &original, &call(&repo, NOTE), &bytes);
}

#[test]
fn ambiguous_legacy_identity_fails_before_publishing_a_note() {
    let (repo, original, bytes) = interrupted_legacy_note();
    let config = prepend_notes_source(&repo, ".memq/notes");
    let output = command(&repo).args(NOTE).output().unwrap();
    let response: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        !output.status.success(),
        "ambiguous retry published a note: {response}"
    );
    assert_eq!(response["error"]["code"], "note_recovery_failed");
    assert!(!repo.root.join(original["path"].as_str().unwrap()).exists());
    assert_eq!(
        fs::read_dir(repo.root.join(".memq/notes")).unwrap().count(),
        0
    );
    let db = rusqlite::Connection::open(repo.db_path()).unwrap();
    let (state, stored, id): (String, String, Option<String>) = db
        .query_row("SELECT state,note_json,item_id FROM note_ops", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap();
    assert_eq!(state, "writing");
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap(),
        serde_json::from_slice::<Value>(&bytes).unwrap()
    );
    assert!(id.is_none());
    drop(db);
    fs::write(repo.root.join(".memq/config.toml"), config).unwrap();
    call(&repo, &["reconcile", "--allow-source-removal"]);
    assert_original_note(&repo, &original, &call(&repo, NOTE), &bytes);
}

#[test]
fn interrupted_legacy_recovery_retains_the_resolved_identity() {
    let (repo, original, bytes) = interrupted_legacy_note();
    prepend_notes_source(&repo, ".memq/extra-notes");
    let output = command(&repo)
        .env("MEMQ_FAULT", "after_note_tmp")
        .args(NOTE)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86));
    assert!(!repo.root.join(original["path"].as_str().unwrap()).exists());
    let db = rusqlite::Connection::open(repo.db_path()).unwrap();
    let id: Option<String> = db
        .query_row("SELECT item_id FROM note_ops", [], |row| row.get(0))
        .unwrap();
    assert_eq!(id.as_deref(), original["id"].as_str());
    drop(db);
    let retry = call(&repo, NOTE);
    assert_original_note(&repo, &original, &retry, &bytes);
    assert!(
        !repo
            .root
            .join(format!(
                ".memq/notes/.{}.tmp",
                original["note_id"].as_str().unwrap()
            ))
            .exists()
    );
    call(&repo, &["rebuild"]);
    assert_original_note(&repo, &original, &call(&repo, NOTE), &bytes);
}
