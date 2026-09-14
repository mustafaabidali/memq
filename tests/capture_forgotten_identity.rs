mod support;

use memq::capture::{self, CaptureCacheInput, Captured};
use memq::config::Config;
use memq::repository::Repository;
use memq::store::Store;
use memq::tombstone::{Predicate, Tombstone};
use memq::util;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::{Repo, expanded_items};
use tempfile::NamedTempFile;

const INSIDE: &str = "2026-09-13T12:00:00Z";
const OUTSIDE: &str = "2026-09-15T12:00:00Z";
const FORGOTTEN: &str = "Synthetic forgotten capture body";
const RETAINED: &str = "Synthetic distinct retained evidence";
const BRIEF: &[&str] = &[
    "brief",
    "--compact",
    "--budget-kind",
    "bytes",
    "--budget",
    "30000",
];

fn run(repo: &Repo, args: &[&str]) -> Value {
    let stdout = NamedTempFile::new().unwrap();
    let stderr = NamedTempFile::new().unwrap();
    let mut command = repo.command();
    for name in [
        "MEMQ_OMP_STORE",
        "MEMQ_CODEX_STORE",
        "MEMQ_OPENCODE_STORE",
        "MEMQ_CODE_REPORT",
        "MEMQ_FAULT",
        "MEMQ_PAUSE_AT",
        "MEMQ_PAUSE_FILE",
    ] {
        command.env_remove(name);
    }
    let mut child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout.reopen().unwrap())
        .stderr(stderr.reopen().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("synthetic capture command exceeded its deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = fs::read(stdout.path()).unwrap();
    assert!(
        status.success(),
        "capture command failed: {}\n{}",
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&fs::read(stderr.path()).unwrap())
    );
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    if value["budget"].is_object() {
        support::budget_check(&bytes);
    }
    value
}

struct Native {
    fixture: Repo,
    repo: Repository,
    config: Config,
    kind: &'static str,
    path: PathBuf,
}

impl Native {
    fn new(kind: &'static str) -> Self {
        let fixture = Repo::new();
        run(&fixture, &["init"]);
        let root = fixture.temp.path().join("sessions");
        fs::create_dir(&root).unwrap();
        let path = root.join(if kind == "opencode" {
            "capture.sqlite"
        } else {
            "session.jsonl"
        });
        let source_path = if kind == "opencode" { &path } else { &root };
        fixture.add_source(&format!("[capture]\n{kind}={}", json!(source_path)));
        let repo = Repository::discover(&fixture.root).unwrap();
        let config =
            Config::parse(&fs::read_to_string(fixture.root.join(".memq/config.toml")).unwrap())
                .unwrap();
        if kind == "opencode" {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,version TEXT,time_updated INTEGER);
                 CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_updated INTEGER,data TEXT);
                 CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);",
            ).unwrap();
            db.execute(
                "INSERT INTO session VALUES('forget-session',?1,'1.18.30',1000)",
                [fixture.root.to_str().unwrap()],
            )
            .unwrap();
            db.execute(
                "INSERT INTO message VALUES('message-one','forget-session',1000,?1)",
                [json!({"role":"assistant"}).to_string()],
            )
            .unwrap();
        }
        Self {
            fixture,
            repo,
            config,
            kind,
            path,
        }
    }

    fn write(&self, records: &[(u64, &str, &str)]) {
        if self.kind == "opencode" {
            let db = Connection::open(&self.path).unwrap();
            db.execute("DELETE FROM part", []).unwrap();
            for (id, body, timestamp) in records {
                let created = chrono::DateTime::parse_from_rfc3339(timestamp)
                    .unwrap()
                    .timestamp_millis();
                db.execute(
                    "INSERT INTO part VALUES(?1,'forget-session','message-one',?2,1000,?3)",
                    params![
                        id.to_string(),
                        created,
                        json!({"type":"text","text":body}).to_string()
                    ],
                )
                .unwrap();
            }
        } else {
            let header = if self.kind == "omp" {
                json!({"type":"session","version":3,"id":"forget-session",
                    "cwd":self.fixture.root,"git":{"branch":"main"}})
            } else {
                json!({"type":"session_meta","ordinal":0,"payload":{"id":"forget-session",
                    "cwd":self.fixture.root,"git":{"branch":"main"}}})
            };
            let mut bytes = format!("{header}\n");
            for (id, body, timestamp) in records {
                let record = if self.kind == "omp" {
                    json!({"type":"message","id":id.to_string(),"timestamp":timestamp,
                        "message":{"content":body}})
                } else {
                    json!({"type":"response_item","ordinal":id,"timestamp":timestamp,
                        "payload":{"text":body}})
                };
                bytes.push_str(&format!("{record}\n"));
            }
            fs::write(&self.path, bytes).unwrap();
        }
    }

    fn retime(&self) {
        if self.kind == "opencode" {
            let created = chrono::DateTime::parse_from_rfc3339(OUTSIDE)
                .unwrap()
                .timestamp_millis();
            Connection::open(&self.path)
                .unwrap()
                .execute("UPDATE part SET time_created=?1 WHERE id='1'", [created])
                .unwrap();
        } else {
            let bytes = fs::read_to_string(&self.path).unwrap();
            assert_eq!(bytes.matches(INSIDE).count(), 1);
            fs::write(&self.path, bytes.replace(INSIDE, OUTSIDE)).unwrap();
        }
    }

    fn collect(&self, cache: &CaptureCacheInput, tombstones: &[Tombstone]) -> Captured {
        capture::collect(
            &self.repo,
            &self.config.project_id,
            &self.config.capture,
            tombstones,
            cache,
        )
        .unwrap()
    }

    fn cache(&self) -> CaptureCacheInput {
        Store::open(&self.fixture.store_root(), &self.repo.clone_id, false)
            .unwrap()
            .capture_cache()
            .unwrap()
    }

    fn window(&self) -> Tombstone {
        let scope = Predicate {
            item_id: None,
            project_id: Some(self.config.project_id.clone()),
            source_id: Some(format!("harness-{}", self.kind)),
            after: Some("2026-09-13T00:00:00Z".into()),
            before: Some("2026-09-14T00:00:00Z".into()),
        };
        Tombstone {
            format: 1,
            identity_hash: scope.hash().unwrap(),
            scope,
            forgotten_at: "2026-09-13T20:00:00Z".into(),
            purge_epoch: util::id(),
        }
    }
}

fn no_stored_body(native: &Native, id: &str) {
    let db = Connection::open(native.fixture.db_path()).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM item_versions WHERE item_id=?1 OR instr(payload_json,?2)>0",
            params![id, FORGOTTEN],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "forgotten capture body was stored again");
}

fn timestamp_edit_after_forget(kind: &'static str) {
    let native = Native::new(kind);
    native.write(&[(1, FORGOTTEN, INSIDE)]);
    let first = run(&native.fixture, BRIEF);
    let items = expanded_items(&first);
    assert_eq!(items.len(), 1);
    let id = items[0]["id"].as_str().unwrap();
    let source = format!("harness-{kind}");
    run(
        &native.fixture,
        &[
            "forget",
            "--source",
            &source,
            "--after",
            "2026-09-13T00:00:00Z",
            "--before",
            "2026-09-14T00:00:00Z",
        ],
    );
    no_stored_body(&native, id);
    native.retime();
    let after = run(&native.fixture, BRIEF);
    assert_eq!(after["coverage"]["capture"][&source]["accepted"], 0);
    assert!(expanded_items(&after).is_empty());
    no_stored_body(&native, id);
    let cache = native.cache();
    assert!(cache.forgotten_ids.contains(id));
    let tombstones = [native.window()];
    let direct = native.collect(&cache, &tombstones);
    assert!(direct.items.is_empty());
    assert!(direct.cache_input().referenced_versions().is_empty());
    assert!(direct.cache_input().payloads_by_version.is_empty());

    // An unobserved identity is still governed by the time predicate, while a
    // distinct record outside that interval remains eligible.
    native.write(&[
        (1, FORGOTTEN, OUTSIDE),
        (
            2,
            "Synthetic unobserved record in the forgotten interval",
            INSIDE,
        ),
        (3, RETAINED, OUTSIDE),
    ]);
    let next = run(&native.fixture, BRIEF);
    let items = expanded_items(&next);
    assert_eq!(items.len(), 1);
    assert!(items[0]["text"].as_str().unwrap().contains(RETAINED));
    assert_eq!(next["coverage"]["capture"][&source]["accepted"], 1);
    assert_eq!(
        run(&native.fixture, BRIEF)["freshness"]["view_id"],
        next["freshness"]["view_id"]
    );
    let direct = native.collect(&native.cache(), &tombstones);
    assert_eq!(direct.items.len(), 1);
    assert!(direct.items[0].payload.redacted_text.contains(RETAINED));
    no_stored_body(&native, id);
}

#[test]
fn omp_timestamp_edit_cannot_restore_a_forgotten_identity() {
    timestamp_edit_after_forget("omp");
}

#[test]
fn codex_timestamp_edit_cannot_restore_a_forgotten_identity() {
    timestamp_edit_after_forget("codex");
}

#[test]
fn opencode_timestamp_edit_cannot_restore_a_forgotten_identity() {
    timestamp_edit_after_forget("opencode");
}

#[test]
fn forgotten_prefix_ids_precede_payload_lookup_and_survive_candidate_reuse() {
    for kind in ["omp", "codex"] {
        let native = Native::new(kind);
        native.write(&[(1, FORGOTTEN, OUTSIDE)]);
        let first = native.collect(&CaptureCacheInput::default(), &[]);
        let forgotten = &first.items[0];
        let tombstones = [native.window()];
        assert!(!tombstones[0].scope.matches(
            &forgotten.id,
            &forgotten.source_id,
            forgotten.observation.recorded_at.as_deref(),
        ));
        native.write(&[
            (1, FORGOTTEN, OUTSIDE),
            (
                2,
                "Synthetic unobserved record in the forgotten interval",
                INSIDE,
            ),
            (3, RETAINED, OUTSIDE),
        ]);
        for missing_payload in [false, true] {
            let mut cache = first.cache_input();
            cache.forgotten_ids.insert(forgotten.id.clone());
            if missing_payload {
                cache.payloads_by_version.clear();
            }
            let warm = native.collect(&cache, &tombstones);
            assert!(!warm.incomplete);
            assert_eq!(warm.items.len(), 1);
            assert!(warm.items[0].payload.redacted_text.contains(RETAINED));
            assert_eq!(
                warm.work.parsed_values, 2,
                "the verified prefix was reparsed"
            );
            assert_eq!(
                warm.work.verified_prefix_bytes,
                first.cursors[0].2["byte_offset"]
            );
            let candidate = warm.cache_input();
            assert_eq!(candidate.forgotten_ids, cache.forgotten_ids);
            assert_eq!(candidate.referenced_versions().len(), 1);
            assert!(
                !candidate
                    .referenced_versions()
                    .contains(&forgotten.version_id)
            );
            assert!(
                !candidate
                    .payloads_by_version
                    .contains_key(&forgotten.version_id)
            );
            let cold = native.collect(
                &CaptureCacheInput {
                    forgotten_ids: cache.forgotten_ids.clone(),
                    ..CaptureCacheInput::default()
                },
                &tombstones,
            );
            assert_eq!(warm.inventory, cold.inventory);
            assert_eq!(warm.coverage, cold.coverage);
            assert_eq!(warm.items[0].version_id, cold.items[0].version_id);
            let repeated = native.collect(&candidate, &tombstones);
            assert_eq!(repeated.inventory, warm.inventory);
            assert_eq!(repeated.coverage, warm.coverage);
            assert_eq!(repeated.work.parsed_values, 0);
            assert_eq!(repeated.work.redacted_records, 0);
            assert_eq!(repeated.work.reused_records, 1);
            assert_eq!(repeated.items[0].version_id, warm.items[0].version_id);
        }
    }
}
