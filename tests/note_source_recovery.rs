mod support;

use memq::store::MutationLock;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::process::{Command, Output, Stdio};
use support::{Repo, expanded_items};

const FIRST: &str = "Synthetic orchard starting evidence";
const NEXT: &str = "Synthetic orchard next evidence";
const KEY: &str = "orchard-first";

#[derive(Clone, Copy, Debug)]
enum Transport {
    Cli,
    Structured,
    Text,
}

const TRANSPORTS: [Transport; 3] = [Transport::Cli, Transport::Structured, Transport::Text];

fn command(repo: &Repo, missing_codex: bool) -> Command {
    let mut command = repo.command();
    for name in ["MEMQ_OMP_STORE", "MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
        command.env(name, "");
    }
    if missing_codex {
        command.env("MEMQ_CODEX_STORE", repo.temp.path().join("unused-codex"));
    }
    for name in [
        "MEMQ_CODE_REPORT",
        "MEMQ_FAULT",
        "MEMQ_PAUSE_AT",
        "MEMQ_PAUSE_FILE",
    ] {
        command.env_remove(name);
    }
    command
}

fn checked(raw: &[u8]) -> Value {
    let value: Value = serde_json::from_slice(raw).unwrap();
    if value.get("budget").is_some() {
        support::budget_check(raw);
    }
    value
}

fn cli_output(output: Output) -> (bool, Value) {
    let value = checked(&output.stdout);
    assert_eq!(output.status.success(), value.get("error").is_none());
    (output.status.success(), value)
}

fn cli(repo: &Repo, missing_codex: bool, args: &[&str]) -> (bool, Value) {
    cli_output(command(repo, missing_codex).args(args).output().unwrap())
}

fn initialized(missing_codex: bool) -> Repo {
    let repo = Repo::new();
    success(cli(&repo, missing_codex, &["init"]));
    repo
}

fn tool(
    repo: &Repo,
    missing_codex: bool,
    transport: Transport,
    name: &str,
    arguments: Value,
) -> (bool, Value) {
    let mut command = command(repo, missing_codex);
    if matches!(transport, Transport::Cli) {
        command.arg(name);
        if name == "note" {
            command.args([
                "--text",
                arguments["text"].as_str().unwrap(),
                "--idempotency-key",
                arguments["idempotency_key"].as_str().unwrap(),
            ]);
        } else {
            command.args(["--compact", "--budget-kind", "bytes", "--budget", "40000"]);
        }
        return cli_output(command.output().unwrap());
    }
    command.arg("mcp");
    if matches!(transport, Transport::Text) {
        command.arg("--text-fallback");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[1]["id"], 2);
    assert!(responses[1].get("error").is_none());
    let result = &responses[1]["result"];
    let raw = if matches!(transport, Transport::Text) {
        assert!(result.get("structuredContent").is_none());
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .as_bytes()
            .to_vec()
    } else {
        assert_eq!(result["content"], json!([]));
        serde_json::to_vec(&result["structuredContent"]).unwrap()
    };
    let value = checked(&raw);
    let ok = !result["isError"].as_bool().unwrap();
    assert_eq!(ok, value.get("error").is_none());
    (ok, value)
}

fn brief(repo: &Repo, missing_codex: bool, via: Transport) -> (bool, Value) {
    tool(
        repo,
        missing_codex,
        via,
        "brief",
        json!({"compact":true,"budget_kind":"bytes","budget":40000}),
    )
}

fn note(repo: &Repo, missing_codex: bool, via: Transport, text: &str, key: &str) -> (bool, Value) {
    tool(
        repo,
        missing_codex,
        via,
        "note",
        json!({"text":text,"idempotency_key":key}),
    )
}

fn success((ok, value): (bool, Value)) -> Value {
    assert!(ok, "{value}");
    value
}

fn failure((ok, value): (bool, Value), code: &str) -> Value {
    assert!(!ok, "{value}");
    assert_eq!(value["error"]["code"], code, "{value}");
    value
}

fn durable(repo: &Repo, value: &Value, text: &str) {
    assert_eq!(value["durability"], "durable", "{value}");
    let bytes = fs::read(repo.root.join(value["path"].as_str().unwrap())).unwrap();
    let authored: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(authored["text"], text);
    assert_eq!(authored["id"], value["note_id"]);
}

fn gap(coverage: &Value, field: &str, source: &str) {
    assert!(
        coverage[field]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"] == source && entry["reason"].is_string()),
        "{coverage}"
    );
}

#[test]
fn missing_unused_adapter_allows_empty_brief_forgotten_retry_and_new_note() {
    for via in TRANSPORTS {
        let repo = initialized(true);
        assert!(!repo.temp.path().join("unused-codex").exists());
        let first = success(note(&repo, true, via, FIRST, KEY));
        durable(&repo, &first, FIRST);
        let id = first["id"].as_str().unwrap();
        let original = fs::read(repo.root.join(first["path"].as_str().unwrap())).unwrap();
        let before = success(brief(&repo, true, via));
        assert_eq!(expanded_items(&before).len(), 1);
        success(cli(&repo, true, &["forget", id]));

        let empty = success(brief(&repo, true, via));
        assert!(expanded_items(&empty).is_empty());
        assert_eq!(empty["freshness"]["status"], "incomplete");
        assert_eq!(empty["incomplete"], true);
        assert_ne!(empty["reason"], "recovery_limit_reached");
        assert_eq!(
            empty["coverage"]["capture"]["harness-codex"]["status"],
            "source_missing"
        );
        gap(&empty["coverage"], "sources_missing", "harness-codex");
        assert_eq!(
            success(note(&repo, true, via, FIRST, KEY)),
            json!({"id":id,"availability":"forgotten"})
        );
        failure(
            note(
                &repo,
                true,
                via,
                "Synthetic conflicting orchard payload",
                KEY,
            ),
            "idempotency_conflict",
        );
        let next = success(note(&repo, true, via, NEXT, "orchard-next"));
        durable(&repo, &next, NEXT);
        assert_ne!(next["id"], first["id"]);
        let after = success(brief(&repo, true, via));
        let items = expanded_items(&after);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], next["id"]);
        assert!(!after.to_string().contains(FIRST));
        assert_eq!(
            fs::read(repo.root.join(first["path"].as_str().unwrap())).unwrap(),
            original
        );
        assert_eq!(
            fs::read_dir(repo.root.join(".memq/notes")).unwrap().count(),
            2
        );
    }
}

#[test]
fn real_source_loss_keeps_read_recovery_guard_and_discloses_durable_note_gap() {
    for via in TRANSPORTS {
        for kind in ["configured", "notes", "paused"] {
            for index_lost in [false, true] {
                let repo = initialized(false);
                let (source, field) = match kind {
                    "configured" => {
                        // A configured repository source may legitimately have this prefix.
                        repo.add_source(
                            "[[source]]\nid='harness-guide'\nkind='markdown'\npath='guide.md'\n",
                        );
                        repo.write("guide.md", FIRST);
                        ("harness-guide", "sources_missing")
                    }
                    "notes" => {
                        success(note(&repo, false, via, FIRST, KEY));
                        ("notes", "sources_missing")
                    }
                    _ => {
                        repo.record_source();
                        repo.records(json!([{"id":"original","text":FIRST}]));
                        ("records", "sources_paused")
                    }
                };
                let before = success(brief(&repo, false, via));
                let original_id = expanded_items(&before)[0]["id"].clone();
                match kind {
                    "configured" => fs::remove_file(repo.root.join("guide.md")).unwrap(),
                    "notes" => fs::remove_dir_all(repo.root.join(".memq/notes")).unwrap(),
                    _ => repo.write("records.json", "{"),
                }
                let stale = success(brief(&repo, false, via));
                assert_eq!(stale["freshness"]["status"], "stale");
                assert_eq!(
                    stale["freshness"]["view_id"],
                    before["freshness"]["view_id"]
                );
                assert_eq!(stale["reason"], "recovery_limit_reached");
                assert_eq!(expanded_items(&stale)[0]["id"], original_id);
                assert!(stale.to_string().contains(FIRST));
                gap(&stale["coverage"], field, source);
                if index_lost {
                    fs::remove_file(repo.db_path()).unwrap();
                    let error = failure(brief(&repo, false, via), "recovery_limit_reached");
                    gap(&error["error"]["detail"]["coverage"], field, source);
                }
                // Catching the read recovery guard must not bypass note validation.
                failure(
                    note(&repo, false, via, "", "invalid-empty-note"),
                    "invalid_note",
                );
                let saved = success(note(&repo, false, via, NEXT, "recovery-next"));
                durable(&repo, &saved, NEXT);
                assert_eq!(saved["recovery"]["code"], "recovery_limit_reached");
                gap(&saved["recovery"]["detail"]["coverage"], field, source);
                assert!(saved["recovery"]["detail"]["reason"].is_string());
                let after = success(brief(&repo, false, via));
                let items = expanded_items(&after);
                assert!(items.iter().any(|item| item["id"] == saved["id"]));
                assert!(items.iter().all(|item| item["id"] != original_id));
                if kind != "notes" {
                    assert_eq!(after["freshness"]["status"], "incomplete");
                    gap(&after["coverage"], field, source);
                }
            }
        }
    }
}

#[test]
fn note_never_bypasses_schema_tombstone_deletion_ledger_or_writer_lock() {
    for via in TRANSPORTS {
        for damage in ["schema", "tombstone", "ledger", "lock"] {
            let repo = initialized(false);
            let root = repo.store_root();
            let staged = repo.git(&["diff", "--cached", "--name-only"]);
            let mut held_lock = None;
            let code = match damage {
                "schema" => {
                    rusqlite::Connection::open(repo.db_path())
                        .unwrap()
                        .execute_batch("PRAGMA user_version=999;")
                        .unwrap();
                    "unsupported_schema"
                }
                "tombstone" => {
                    repo.write(".memq/tombstones/invalid.json", "{}");
                    "invalid_tombstone"
                }
                "ledger" => {
                    fs::remove_file(root.join("access.sqlite")).unwrap();
                    "access_state_incomplete"
                }
                _ => {
                    held_lock = Some(MutationLock::acquire(&root).unwrap());
                    "in_progress"
                }
            };
            failure(note(&repo, false, via, NEXT, "blocked-write"), code);
            let notes = repo.root.join(".memq/notes");
            if notes.exists() {
                assert_eq!(fs::read_dir(notes).unwrap().count(), 0);
            }
            assert_eq!(repo.git(&["diff", "--cached", "--name-only"]), staged);
            if damage == "ledger" {
                assert!(!root.join("access.sqlite").exists());
            }
            drop(held_lock);
        }
    }
}
