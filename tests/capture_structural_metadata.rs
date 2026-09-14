mod support;

use memq::{capture, config::Config, repository::Repository, store::Store};
use rusqlite::{Connection, types::ValueRef};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use support::{Repo, expanded_items};
use tempfile::NamedTempFile;

const DESCRIPTION: &str = "PROJ-4821-Refactor-OAuth2-Token-Refresh-Handling-For-Mobile-Clients";
const BRIEF: &[&str] = &["brief", "--budget-kind", "bytes", "--budget", "30000"];
const OPAQUE: &str = "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cD0fG";

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

fn journal(root: &Path, kind: &str, cwd: &Path, branch: &str) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    if kind == "opencode" {
        let path = root.join("capture.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,version TEXT,time_updated INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_updated INTEGER,data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);",
        ).unwrap();
        db.execute(
            "INSERT INTO session VALUES('session-one',?1,'1.18.30',1000)",
            [cwd.to_str().unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES('message-one','session-one',1000,?1)",
            [json!({"role":"assistant"}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES('part-one','session-one','message-one',1000,1000,?1)",
            [json!({"type":"text","text":"Synthetic structural evidence"}).to_string()],
        )
        .unwrap();
        path
    } else {
        let git = json!({"branch":branch,"api_key":format!("ghp_{}", "x".repeat(36))});
        let (header, record) = if kind == "omp" {
            (
                json!({"type":"session","version":3,"id":"session-one","cwd":cwd,"git":git}),
                json!({"type":"message","id":"entry-one","message":{"content":"Synthetic structural evidence"}}),
            )
        } else {
            (
                json!({"type":"session_meta","ordinal":0,"payload":{"id":"session-one","cwd":cwd,"git":git}}),
                json!({"type":"response_item","ordinal":1,"payload":{"text":"Synthetic structural evidence"}}),
            )
        };
        fs::write(root.join("session.jsonl"), format!("{header}\n{record}\n")).unwrap();
        root.to_owned()
    }
}

fn configure(repo: &Repo, kind: &str, path: &Path) {
    repo.add_source(&format!("[capture]\n{kind}={}", json!(path)));
}

fn no_persisted_secret(repo: &Repo, secret: &str) {
    let db = Connection::open(repo.db_path()).unwrap();
    let tables = db
        .prepare("SELECT name FROM sqlite_schema WHERE type='table'")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for table in tables {
        let mut stmt = db
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            for column in 0..columns {
                if let ValueRef::Text(text) = row.get_ref(column).unwrap() {
                    assert!(
                        !String::from_utf8_lossy(text).contains(secret),
                        "sensitive structural value persisted in {table}"
                    );
                }
            }
        }
    }
}

fn rejected(repo: &Repo, kind: &str, secret: &str) {
    let response = run(repo, BRIEF);
    assert_eq!(response["coverage"]["capture_incomplete"], true);
    assert!(
        response["coverage"]["capture"][format!("harness-{kind}")]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["reason"] == "capture_unsafe_metadata")
    );
    assert!(!response.to_string().contains(secret));
    assert!(expanded_items(&response).is_empty());
    no_persisted_secret(repo, secret);
}

#[test]
fn descriptive_branches_preserve_revision_and_verified_prefix_reuse() {
    for kind in ["omp", "codex"] {
        let repo = Repo::new();
        let branch = format!("feature/{DESCRIPTION}");
        repo.git(&["checkout", "-q", "-b", &branch]);
        run(&repo, &["init"]);
        let source = journal(
            &repo.temp.path().join("sessions"),
            kind,
            &repo.root,
            &branch,
        );
        configure(&repo, kind, &source);
        let first = run(&repo, BRIEF);
        let items = expanded_items(&first);
        assert_eq!(items.len(), 1, "descriptive branch was rejected");
        assert_eq!(items[0]["observation"]["branch"], branch);
        assert_eq!(items[0]["observation"]["native_revision"]["branch"], branch);
        assert_eq!(
            items[0]["observation"]["native_revision"]["api_key"],
            "[REDACTED]"
        );
        assert_eq!(first["coverage"]["capture_incomplete"], false);
        assert_eq!(
            run(&repo, BRIEF)["freshness"]["view_id"],
            first["freshness"]["view_id"]
        );
        no_persisted_secret(&repo, &format!("ghp_{}", "x".repeat(36)));
        let discovered = Repository::discover(&repo.root).unwrap();
        let config =
            Config::parse(&fs::read_to_string(repo.root.join(".memq/config.toml")).unwrap())
                .unwrap();
        let cache = Store::open(&repo.store_root(), &discovered.clone_id, false)
            .unwrap()
            .capture_cache()
            .unwrap();
        let warm = capture::collect(
            &discovered,
            &config.project_id,
            &config.capture,
            &[],
            &cache,
        )
        .unwrap();
        assert_eq!(warm.work.parsed_values, 0);
        assert_eq!(warm.work.reused_records, 1);
        assert_eq!(warm.items[0].observation.branch, branch);
    }
}

#[test]
fn descriptive_path_components_are_preserved_across_native_adapters() {
    for kind in ["omp", "codex", "opencode"] {
        for component in ["cwd", "worktree", "root", "file"] {
            let mut repo = Repo::new();
            if component == "worktree" {
                let path = repo.temp.path().join(DESCRIPTION);
                fs::rename(&repo.root, &path).unwrap();
                repo.root = path;
            }
            run(&repo, &["init"]);
            let cwd = if component == "cwd" {
                let path = repo.root.join(DESCRIPTION);
                fs::create_dir(&path).unwrap();
                path
            } else {
                repo.root.clone()
            };
            let root = repo.temp.path().join(if component == "root" {
                DESCRIPTION
            } else {
                "sessions"
            });
            let mut source = journal(&root, kind, &cwd, "main");
            if component == "file" {
                if kind == "opencode" {
                    let renamed = root.join(format!("{DESCRIPTION}.sqlite"));
                    fs::rename(&source, &renamed).unwrap();
                    source = renamed;
                } else {
                    fs::rename(
                        root.join("session.jsonl"),
                        root.join(format!("{DESCRIPTION}.jsonl")),
                    )
                    .unwrap();
                }
            }
            configure(&repo, kind, &source);
            let response = run(&repo, BRIEF);
            assert_eq!(
                expanded_items(&response).len(),
                1,
                "{kind} rejected {component}"
            );
            assert_eq!(response["coverage"]["capture_incomplete"], false);
            assert_eq!(
                run(&repo, BRIEF)["freshness"]["view_id"],
                response["freshness"]["view_id"]
            );
        }
    }
}

#[test]
fn task_and_disk_words_are_preserved_in_branches_and_path_components() {
    for (branch, component) in [
        (
            "feature/task-improve-session-capture",
            "task-improve-session-capture",
        ),
        (
            "fix/disk-cache-invalidation-on-retry",
            "disk-cache-invalidation-on-retry",
        ),
    ] {
        for kind in ["omp", "codex", "opencode"] {
            let mut repo = Repo::new();
            let worktree = repo.temp.path().join(component);
            fs::rename(&repo.root, &worktree).unwrap();
            repo.root = worktree;
            repo.git(&["checkout", "-q", "-b", branch]);
            run(&repo, &["init"]);
            let cwd = repo.root.join(component);
            fs::create_dir(&cwd).unwrap();
            let root = repo.temp.path().join("sessions").join(component);
            let mut source = journal(&root, kind, &cwd, branch);
            if kind == "opencode" {
                let renamed = root.join(format!("{component}.sqlite"));
                fs::rename(&source, &renamed).unwrap();
                source = renamed;
            } else {
                fs::rename(
                    root.join("session.jsonl"),
                    root.join(format!("{component}.jsonl")),
                )
                .unwrap();
            }
            configure(&repo, kind, &source);
            let mut brief = BRIEF.to_vec();
            brief.push("--compact");
            let first = run(&repo, &brief);
            let items = expanded_items(&first);
            assert_eq!(items.len(), 1, "{kind} rejected {component}");
            assert_eq!(first["coverage"]["capture_incomplete"], false);
            if kind != "opencode" {
                assert_eq!(items[0]["observation"]["branch"], branch);
                assert_eq!(items[0]["observation"]["native_revision"]["branch"], branch);
            }
            assert_eq!(
                run(&repo, &brief)["freshness"]["view_id"],
                first["freshness"]["view_id"],
            );
        }
    }
}

#[test]
fn git_valid_secret_bearing_branches_still_pause_without_persistence() {
    for kind in ["omp", "codex"] {
        let token = format!("ghp_{}", "a".repeat(36));
        let aws = format!("AKIA{}", "X".repeat(16));
        let key = format!("sk-{}", "x".repeat(32));
        let project_key = format!("sk-proj-{}", "x".repeat(32));
        for (branch, secret) in [
            (format!("feature/{token}"), token.as_str()),
            (format!("feature/snapshot_{token}"), token.as_str()),
            (format!("feature/{OPAQUE}"), OPAQUE),
            (format!("feature/snapshot_{aws}"), aws.as_str()),
            (format!("feature/{key}"), key.as_str()),
            (format!("feature/snapshot_{key}"), key.as_str()),
            (format!("feature/task-{key}"), key.as_str()),
            (format!("feature/{project_key}"), project_key.as_str()),
        ] {
            let repo = Repo::new();
            repo.git(&["check-ref-format", "--branch", &branch]);
            run(&repo, &["init"]);
            let source = journal(
                &repo.temp.path().join("sessions"),
                kind,
                &repo.root,
                &branch,
            );
            configure(&repo, kind, &source);
            rejected(&repo, kind, secret);
        }
    }
}

#[test]
fn embedded_credentials_in_cwd_and_source_components_remain_rejected() {
    for secret in [
        format!("ghp_{}", "a".repeat(36)),
        format!("sk-{}", "x".repeat(32)),
    ] {
        let component = format!("snapshot_{secret}");
        for kind in ["omp", "codex", "opencode"] {
            for place in ["cwd", "root", "file"] {
                let repo = Repo::new();
                run(&repo, &["init"]);
                let cwd = if place == "cwd" {
                    let path = repo.root.join(&component);
                    fs::create_dir(&path).unwrap();
                    path
                } else {
                    repo.root.clone()
                };
                let root = repo.temp.path().join(if place == "root" {
                    &component
                } else {
                    "sessions"
                });
                let mut source = journal(&root, kind, &cwd, "main");
                if place == "file" {
                    if kind == "opencode" {
                        let renamed = root.join(format!("{component}.sqlite"));
                        fs::rename(&source, &renamed).unwrap();
                        source = renamed;
                    } else {
                        fs::rename(
                            root.join("session.jsonl"),
                            root.join(format!("{component}.jsonl")),
                        )
                        .unwrap();
                    }
                }
                configure(&repo, kind, &source);
                rejected(&repo, kind, &secret);
            }
        }
    }
}

#[test]
fn opaque_native_ids_keep_the_stricter_text_redaction_policy() {
    for kind in ["omp", "codex", "opencode"] {
        let repo = Repo::new();
        run(&repo, &["init"]);
        let root = repo.temp.path().join("sessions");
        let source = journal(&root, kind, &repo.root, "main");
        if kind == "opencode" {
            Connection::open(&source)
                .unwrap()
                .execute("UPDATE session SET id=?1", [OPAQUE])
                .unwrap();
        } else {
            let path = root.join("session.jsonl");
            let text = fs::read_to_string(&path).unwrap();
            let mut lines = text.lines();
            let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
            if kind == "omp" {
                header["id"] = json!(OPAQUE);
            } else {
                header["payload"]["id"] = json!(OPAQUE);
            }
            fs::write(path, format!("{header}\n{}\n", lines.next().unwrap())).unwrap();
        }
        configure(&repo, kind, &source);
        rejected(&repo, kind, OPAQUE);
    }
}
