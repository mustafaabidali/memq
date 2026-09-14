mod support;

use memq::repository::Repository;
use memq::store::{MutationLock, Store};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use support::{Repo, expanded_items};

const INSIDE: &str = "2026-09-13T12:00:00Z";
const OUTSIDE: &str = "2026-09-15T12:00:00Z";
const BODY: &str = "Synthetic cobalt orchard erasedcontent";
const KEPT: &str = "Synthetic cobalt retained meadow";
const NEW: &str = "Synthetic cobalt distinct later evidence";
const BLOCKED: &str = "Synthetic newly encountered in-range evidence";

struct Fixture {
    repo: Repo,
    capture: bool,
    sessions: PathBuf,
    model_inputs: PathBuf,
}

impl Fixture {
    fn new(capture: bool) -> Self {
        let repo = Repo::new();
        let sessions = repo.temp.path().join("sessions");
        fs::create_dir(&sessions).unwrap();
        let model_inputs = repo.temp.path().join("model-inputs.jsonl");
        let fixture = Self {
            repo,
            capture,
            sessions,
            model_inputs,
        };
        fixture.call(&["init"]);
        if !capture {
            fixture.repo.record_source();
        }
        let script = fixture.repo.temp.path().join("observe-embedding.py");
        fs::write(
            &script,
            format!(
                r#"import io, json, sys
request_bytes = sys.stdin.read()
with open(sys.argv[1], 'a') as output:
    output.write(json.dumps(json.loads(request_bytes)) + '\n')
sys.stdin = io.StringIO(request_bytes)
{}"#,
                include_str!("fixtures/embedding.py"),
            ),
        )
        .unwrap();
        fixture.repo.add_source(&format!(
            "[vectors]\ncommand=['python3',{},{}]\nmodel='test-plumbing-only'\n\
             dims=4\npreprocessing_version='fixture-v1'\ntimeout_seconds=5\n",
            json!(script),
            json!(fixture.model_inputs),
        ));
        fixture
    }

    fn command(&self) -> Command {
        // Allow the same public regression to run against a frozen binary.
        let binary = std::env::var_os("MEMQ_FORGOTTEN_IDENTITY_TEST_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_memq").into());
        let mut command = Command::new(binary);
        command
            .arg("--repo")
            .arg(&self.repo.root)
            .env("MEMQ_DATA_DIR", &self.repo.data)
            .env("MEMQ_NOW", "2026-09-13T20:00:00Z")
            .env("MEMQ_OMP_STORE", &self.sessions)
            // Only OMP participates here. Missing unrelated adapters would
            // make an intentionally empty post-forget view test recovery.
            .env("MEMQ_CODEX_STORE", "")
            .env("MEMQ_OPENCODE_STORE", "");
        for name in [
            "MEMQ_CODE_REPORT",
            "MEMQ_FAULT",
            "MEMQ_PAUSE_AT",
            "MEMQ_PAUSE_FILE",
            "MEMQ_TEST_EMBED_DELAY",
        ] {
            command.env_remove(name);
        }
        command
    }

    fn call(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().unwrap();
        assert!(
            output.status.success(),
            "public command {args:?} failed: {:?}\n{}\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        if response.get("budget").is_some() {
            support::budget_check(&output.stdout);
        }
        response
    }

    fn write(&self, timestamp: &str, new_records: bool) {
        let mut records = vec![("erased", BODY, timestamp), ("kept", KEPT, OUTSIDE)];
        if new_records {
            records.extend([("later", NEW, OUTSIDE), ("blocked-new", BLOCKED, INSIDE)]);
        }
        if self.capture {
            let header = json!({
                "type":"session","version":3,"id":"orchard-session",
                "cwd":self.repo.root,"git":{"branch":"main"}
            });
            let mut bytes = format!("{header}\n");
            for (id, text, timestamp) in records {
                let row = json!({
                    "type":"message","id":id,"timestamp":timestamp,
                    "message":{"content":text}
                });
                bytes.push_str(&format!("{row}\n"));
            }
            fs::write(self.sessions.join("orchard.jsonl"), bytes).unwrap();
        } else {
            self.repo.records(Value::Array(
                records
                    .into_iter()
                    .map(|(id, text, timestamp)| {
                        json!({"id":id,"text":text,"recorded_at":timestamp})
                    })
                    .collect(),
            ));
        }
    }

    fn brief(&self) -> Value {
        self.call(&[
            "brief",
            "--compact",
            "--budget-kind",
            "bytes",
            "--budget",
            "40000",
        ])
    }

    fn forget_window(&self) {
        let forgotten = self.call(&[
            "forget",
            "--source",
            if self.capture {
                "harness-omp"
            } else {
                "records"
            },
            "--after",
            "2026-09-13T00:00:00Z",
            "--before",
            "2026-09-14T00:00:00Z",
        ]);
        assert_eq!(forgotten["purge"], "complete");
        fs::write(&self.model_inputs, b"").unwrap();
    }

    fn model_texts(&self) -> Vec<String> {
        fs::read_to_string(&self.model_inputs)
            .unwrap()
            .lines()
            .flat_map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["texts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|text| text.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

fn erased_id(response: &Value) -> String {
    expanded_items(response)
        .iter()
        .find(|item| {
            item["text"]
                .as_str()
                .is_some_and(|text| text.contains(BODY))
        })
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn assert_purged(fixture: &Fixture, id: &str) {
    let access = Connection::open(fixture.repo.store_root().join("access.sqlite")).unwrap();
    assert!(
        access
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM forgotten_items WHERE item_id=?1)",
                [id],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
    drop(access);
    for entry in fs::read_dir(fixture.repo.store_root().join("generations")).unwrap() {
        let path = entry.unwrap().path().join("index.sqlite");
        if !path.exists() {
            continue;
        }
        let db = Connection::open(&path).unwrap();
        for sql in [
            "SELECT count(*) FROM items WHERE item_id=?1",
            "SELECT count(*) FROM item_versions WHERE item_id=?1",
            "SELECT count(*) FROM view_items WHERE item_id=?1",
        ] {
            assert_eq!(
                db.query_row(sql, [id], |row| row.get::<_, i64>(0)).unwrap(),
                0,
                "forgotten identity remains in a retained generation"
            );
        }
        for sql in [
            "SELECT count(*) FROM vector_meta WHERE version_id NOT IN (SELECT version_id FROM item_versions)",
            "SELECT count(*) FROM embed_queue WHERE version_id NOT IN (SELECT version_id FROM item_versions)",
        ] {
            assert_eq!(
                db.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap(),
                0,
                "orphaned embedding state survived identity purge"
            );
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM item_versions WHERE instr(payload_json,?1)>0 OR instr(payload_json,?2)>0",
                [BODY, BLOCKED],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "a forgotten body was persisted under a replacement identity",
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM note_ops WHERE instr(note_json,?1)>0",
                [BODY],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0,
            "a forgotten operation retained its original body",
        );
        drop(db);
        for file in [path.clone(), path.with_extension("sqlite-wal")] {
            if file.exists() {
                assert!(
                    !fs::read(file)
                        .unwrap()
                        .windows(BODY.len())
                        .any(|bytes| bytes == BODY.as_bytes()),
                    "forgotten body bytes remain in closed SQLite storage",
                );
            }
        }
    }
    assert!(
        fixture
            .model_texts()
            .iter()
            .all(|text| !text.contains(BODY) && !text.contains(BLOCKED)),
        "forgotten or time-excluded content reached the embedding command",
    );
}

fn assert_reads_filter(fixture: &Fixture, id: &str, retained_view: &str) {
    for compact in [false, true] {
        for view in [None, Some(retained_view)] {
            for operation in [
                vec!["brief"],
                vec!["search", "erasedcontent"],
                vec!["search", "quartz"],
            ] {
                let mut args = operation;
                args.extend(["--budget-kind", "bytes", "--budget", "40000"]);
                if let Some(view) = view {
                    args.extend(["--view-id", view]);
                }
                if compact {
                    args.push("--compact");
                }
                let response = fixture.call(&args);
                assert!(!response.to_string().contains(BODY));
                let items = expanded_items(&response);
                assert!(items.iter().all(|item| item["id"] != id));
                assert!(
                    items.iter().any(|item| {
                        item["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(KEPT))
                    }),
                    "an unrelated out-of-range identity was lost: {response}",
                );
            }
            let mut args = vec!["show", id, "--budget-kind", "bytes", "--budget", "40000"];
            if let Some(view) = view {
                args.extend(["--view-id", view]);
            }
            if compact {
                args.push("--compact");
            }
            assert_eq!(
                expanded_items(&fixture.call(&args)),
                vec![json!({"id":id,"availability":"forgotten"})],
            );
        }
    }
}

#[test]
fn timestamp_replay_stays_forgotten_through_rebuild_and_lexical_vector_reads() {
    for capture in [true, false] {
        let fixture = Fixture::new(capture);
        fixture.write(INSIDE, false);
        let original = fixture.brief();
        let id = erased_id(&original);
        let retained = original["freshness"]["view_id"].as_str().unwrap();
        assert_eq!(fixture.call(&["embed"])["published"], 2);
        fixture.forget_window();
        fixture.write(OUTSIDE, true);
        for rebuild in [false, true] {
            if rebuild {
                fixture.call(&["rebuild"]);
            }
            let current = fixture.brief();
            assert!(
                expanded_items(&current)
                    .iter()
                    .any(|item| { item["text"].as_str().is_some_and(|text| text.contains(NEW)) })
            );
            assert_purged(&fixture, &id);
            assert!(fixture.call(&["embed"])["published"].as_u64().unwrap() > 0);
            assert_reads_filter(&fixture, &id, retained);
            assert_purged(&fixture, &id);
        }
        assert!(
            fixture.model_texts().iter().any(|text| text.contains(NEW)),
            "the mechanical embedding fixture did not exercise surviving content",
        );
    }
}

#[test]
fn older_in_range_evidence_purges_the_newer_out_of_range_generation() {
    let fixture = Fixture::new(false);
    fixture.write(INSIDE, false);
    let original = fixture.brief();
    let id = erased_id(&original);
    fixture.write(OUTSIDE, false);
    fixture.call(&["rebuild"]);
    fixture.call(&["embed"]);
    fixture.forget_window();
    // Check immediately: a later command must not be needed to finish purging
    // the active generation after an archive established the forgotten ID.
    assert_purged(&fixture, &id);
    assert_reads_filter(
        &fixture,
        &id,
        original["freshness"]["view_id"].as_str().unwrap(),
    );
}

fn caches(fixture: &Fixture) -> BTreeMap<PathBuf, Vec<u8>> {
    walkdir::WalkDir::new(fixture.repo.store_root().join("generations"))
        .into_iter()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry.file_type().is_file()
                && entry.path().extension().is_some_and(|ext| ext == "usearch")
        })
        .map(|entry| (entry.path().to_owned(), fs::read(entry.path()).unwrap()))
        .collect()
}

#[test]
#[cfg(debug_assertions)]
fn restored_out_of_range_rows_resume_vector_cleanup_after_a_crash() {
    let fixture = Fixture::new(true);
    fixture.write(INSIDE, false);
    let original = fixture.brief();
    let id = erased_id(&original);
    fixture.write(OUTSIDE, false);
    fixture.call(&["rebuild"]);
    let current = fixture.brief();
    fixture.call(&["embed"]);
    fixture.call(&["search", "quartz"]);
    let active = fixture.repo.db_path();
    let saved = fixture.repo.temp.path().join("out-of-range.sqlite");
    fs::copy(&active, &saved).unwrap();
    let old_caches = caches(&fixture);
    assert!(!old_caches.is_empty());
    fixture.forget_window();
    assert_purged(&fixture, &id);

    // Simulate the buggy writer restoring valid derived rows with an edited
    // timestamp, while the completed persistent deletion ledger survives.
    fs::copy(&saved, &active).unwrap();
    let archive = fixture
        .repo
        .store_root()
        .join("generations")
        .join(memq::util::id());
    fs::create_dir(&archive).unwrap();
    fs::copy(&saved, archive.join("index.sqlite")).unwrap();
    fs::write(archive.join("VERIFIED"), b"1").unwrap();
    for (path, bytes) in &old_caches {
        fs::write(path, bytes).unwrap();
    }
    let temporary_cache = old_caches.keys().next().unwrap().with_extension("tmp");
    fs::write(&temporary_cache, b"synthetic unfinished vector cache").unwrap();
    let output = fixture
        .command()
        .env("MEMQ_FAULT", "before_vector_purge")
        .arg("brief")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(86));
    assert_purged(&fixture, &id);
    assert_eq!(caches(&fixture), old_caches);
    assert!(temporary_cache.exists());
    fixture.brief();
    assert!(caches(&fixture).is_empty());
    assert!(!temporary_cache.exists());
    assert_reads_filter(
        &fixture,
        &id,
        current["freshness"]["view_id"].as_str().unwrap(),
    );
    assert_purged(&fixture, &id);
    let safe = caches(&fixture);
    assert!(!safe.is_empty());
    fixture.brief();
    assert_eq!(
        caches(&fixture),
        safe,
        "safe caches were needlessly deleted"
    );
}

fn damage_payload(path: &std::path::Path, id: &str) {
    let db = Connection::open(path).unwrap();
    let trigger: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='immutable_versions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    db.execute_batch("DROP TRIGGER immutable_versions;")
        .unwrap();
    db.execute(
        "UPDATE item_versions SET payload_json='invalid json' WHERE item_id=?1",
        [id],
    )
    .unwrap();
    db.execute_batch(&trigger).unwrap();
}

#[test]
fn capture_cache_excludes_forgotten_bodies_before_decoding_payloads() {
    let fixture = Fixture::new(true);
    fixture.write(INSIDE, false);
    let original = fixture.brief();
    let id = erased_id(&original);
    let kept_id = expanded_items(&original)
        .iter()
        .find(|item| item["id"] != id)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = fixture.repo.db_path();
    let saved = fixture.repo.temp.path().join("retained-capture.sqlite");
    fs::copy(&path, &saved).unwrap();
    fixture.forget_window();
    fs::copy(saved, &path).unwrap();
    damage_payload(&path, &id);

    let repo = Repository::discover(&fixture.repo.root).unwrap();
    let _lock = MutationLock::acquire(&fixture.repo.store_root()).unwrap();
    {
        let store = Store::open(&fixture.repo.store_root(), &repo.clone_id, false).unwrap();
        let cache = store.capture_cache().unwrap();
        assert!(cache.forgotten_ids.contains(&id));
        assert_eq!(cache.referenced_versions().len(), 2);
        assert_eq!(cache.payloads_by_version.len(), 1);
        assert!(
            cache
                .payloads_by_version
                .values()
                .all(|payload| payload.redacted_text.contains(KEPT))
        );
    }
    // An unreadable authoritative body for a surviving ID is not an advisory
    // cache miss. The loader must preserve ordinary storage error handling.
    damage_payload(&path, &kept_id);
    let store = Store::open(&fixture.repo.store_root(), &repo.clone_id, false).unwrap();
    assert!(store.capture_cache().is_err());
}

#[test]
fn missing_forgotten_identity_ledger_blocks_timestamp_replay() {
    let fixture = Fixture::new(false);
    fixture.write(INSIDE, false);
    fixture.brief();
    fixture.forget_window();
    fixture.write(OUTSIDE, false);
    let access_path = fixture.repo.store_root().join("access.sqlite");
    let access = Connection::open(&access_path).unwrap();
    access.execute_batch("DROP TABLE forgotten_items;").unwrap();
    drop(access);
    let before = fs::read(&access_path).unwrap();
    for args in [vec!["brief"], vec!["rebuild"], vec!["embed"]] {
        let output = fixture.command().args(args).output().unwrap();
        assert!(!output.status.success());
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["error"]["code"], "access_state_incomplete");
        assert!(!response.to_string().contains(BODY));
    }
    assert_eq!(fs::read(access_path).unwrap(), before);
    assert!(fixture.model_texts().is_empty());
}

#[test]
fn retimed_note_purges_original_operation_body_and_preserves_retry_identity() {
    let fixture = Fixture::new(true);
    let request = [
        "note",
        "--text",
        BODY,
        "--idempotency-key",
        "retimed-orchard",
    ];
    let output = fixture
        .command()
        .env("MEMQ_NOW", OUTSIDE)
        .args(request)
        .output()
        .unwrap();
    assert!(output.status.success());
    let note: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = note["id"].as_str().unwrap();
    let path = note["path"].as_str().unwrap();
    let staged = fixture.repo.git(&["show", &format!(":{path}")]);
    let mut authored: Value =
        serde_json::from_slice(&fs::read(fixture.repo.root.join(path)).unwrap()).unwrap();
    authored["recorded_at"] = json!(INSIDE);
    let bytes = serde_json::to_vec(&authored).unwrap();
    fixture.repo.write(path, &bytes);
    assert_eq!(erased_id(&fixture.brief()), id);
    let forgotten = fixture.call(&[
        "forget",
        "--source",
        "notes",
        "--after",
        "2026-09-13T00:00:00Z",
        "--before",
        "2026-09-14T00:00:00Z",
    ]);
    assert_eq!(forgotten["purge"], "complete");
    fs::write(&fixture.model_inputs, b"").unwrap();
    assert_purged(&fixture, id);
    for rebuild in [false, true] {
        if rebuild {
            fixture.call(&["rebuild"]);
        }
        assert_eq!(
            fixture.call(&request),
            json!({"id":id,"availability":"forgotten"}),
        );
        assert_purged(&fixture, id);
        assert_eq!(fs::read(fixture.repo.root.join(path)).unwrap(), bytes);
        assert_eq!(fixture.repo.git(&["show", &format!(":{path}")]), staged);
        assert_eq!(
            fs::read_dir(fixture.repo.root.join(".memq/notes"))
                .unwrap()
                .count(),
            1,
        );
    }
}
