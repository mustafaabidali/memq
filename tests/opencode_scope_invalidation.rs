mod support;

use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::{Repo, expanded_items};
use tempfile::NamedTempFile;

const BRIEF: &[&str] = &["brief", "--budget-kind", "bytes", "--budget", "30000"];

struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_command(command: &mut Command, mut poll: impl FnMut()) -> Value {
    let stdout = NamedTempFile::new().unwrap();
    let stderr = NamedTempFile::new().unwrap();
    let mut child = Running(
        command
            .stdin(Stdio::null())
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        poll();
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "synthetic capture command exceeded its deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = fs::read(stdout.path()).unwrap();
    assert!(
        status.success(),
        "capture command failed: {}\n{}",
        String::from_utf8_lossy(&bytes),
        String::from_utf8_lossy(&fs::read(stderr.path()).unwrap()),
    );
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    if value["budget"].is_object() {
        support::budget_check(&bytes);
    }
    value
}

struct Session {
    repo: Repo,
    source: PathBuf,
    linked: PathBuf,
}

impl Session {
    fn new() -> Self {
        let mut repo = Repo::new();
        repo.root = fs::canonicalize(&repo.root).unwrap();
        let linked = repo.temp.path().join("detached");
        repo.git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ]);
        let source = repo.temp.path().join("capture.sqlite");
        let db = Connection::open(&source).unwrap();
        db.execute_batch(
            "CREATE TABLE session(id TEXT PRIMARY KEY,directory TEXT,version TEXT,time_updated INTEGER);
             CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_updated INTEGER,data TEXT);
             CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,time_updated INTEGER,data TEXT);",
        ).unwrap();
        db.execute(
            "INSERT INTO session VALUES('scope-session',?1,'1.18.30',1000)",
            [repo.root.to_str().unwrap()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO message VALUES('message-one','scope-session',1000,?1)",
            [json!({"role":"assistant"}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES('part-one','scope-session','message-one',1000,1000,?1)",
            [json!({"type":"text","text":"Synthetic scope evidence"}).to_string()],
        )
        .unwrap();
        drop(db);
        let session = Self {
            repo,
            source,
            linked,
        };
        session.run(&["init"]);
        session
    }

    fn run(&self, args: &[&str]) -> Value {
        run_command(self.command().args(args), || {})
    }

    fn command(&self) -> Command {
        let mut command = self.repo.command();
        for name in [
            "MEMQ_OMP_STORE",
            "MEMQ_CODEX_STORE",
            "MEMQ_CODE_REPORT",
            "MEMQ_FAULT",
            "MEMQ_PAUSE_AT",
            "MEMQ_PAUSE_FILE",
        ] {
            command.env_remove(name);
        }
        command.env("MEMQ_OPENCODE_STORE", &self.source);
        command
    }

    fn directory(&self, directory: &Path) {
        let db = Connection::open(&self.source).unwrap();
        db.execute(
            "UPDATE session SET directory=?1",
            [directory.to_str().unwrap()],
        )
        .unwrap();
        let times: (i64, i64, i64, i64) = db
            .query_row(
                "SELECT s.time_updated,m.time_updated,p.time_created,p.time_updated
                 FROM session s JOIN message m ON m.session_id=s.id
                 JOIN part p ON p.message_id=m.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(times, (1000, 1000, 1000, 1000));
    }

    fn history(&self, before: &Value, item: &Value) -> Value {
        let history = self.run(&[
            "show",
            item["id"].as_str().unwrap(),
            "--view-id",
            before["freshness"]["view_id"].as_str().unwrap(),
            "--budget-kind",
            "bytes",
            "--budget",
            "30000",
        ]);
        assert_eq!(history["freshness"]["status"], "stale");
        assert_eq!(history["freshness"]["reason"], "retained_view");
        assert_eq!(
            history["freshness"]["view_id"],
            before["freshness"]["view_id"]
        );
        let retained = expanded_items(&history);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0]["id"], item["id"]);
        assert_eq!(retained[0]["text"], item["text"]);
        assert_eq!(retained[0]["pointer"], item["pointer"]);
        assert_eq!(
            retained[0]["observation"]["worktree_key"],
            json!(self.repo.root)
        );
        retained[0].clone()
    }
}

fn assert_new_empty_view(before: &Value, after: &Value) {
    assert_ne!(
        after["freshness"]["view_id"], before["freshness"]["view_id"],
        "scope changed but the previous view was reused"
    );
    assert_ne!(
        after["freshness"]["inputs_hash"],
        before["freshness"]["inputs_hash"]
    );
    assert!(
        expanded_items(after).is_empty(),
        "foreign-worktree evidence remained current"
    );
    assert_eq!(after["freshness"]["status"], "current");
    assert_eq!(
        after["coverage"]["capture"]["harness-opencode"]["status"],
        "ok"
    );
}

#[test]
fn mutable_opencode_directory_invalidates_scope_without_timestamp_changes() {
    let session = Session::new();
    let before = session.run(BRIEF);
    let original = expanded_items(&before).remove(0);
    assert_eq!(
        original["observation"]["worktree_key"],
        json!(session.repo.root)
    );
    assert_eq!(
        session.run(BRIEF)["freshness"]["view_id"],
        before["freshness"]["view_id"],
        "unchanged scope must remain reusable"
    );
    session.directory(&session.linked);
    let after = session.run(BRIEF);
    assert_new_empty_view(&before, &after);
    assert_eq!(
        session.run(BRIEF)["freshness"]["view_id"],
        after["freshness"]["view_id"]
    );
    let mut fresh_task = BRIEF.to_vec();
    fresh_task.extend(["--task", "independent-scope-check"]);
    assert!(expanded_items(&session.run(&fresh_task)).is_empty());
    let retained = session.history(&before, &original);
    assert_eq!(retained["version_id"], original["version_id"]);
    assert_eq!(retained["observation"], original["observation"]);
}

#[cfg(unix)]
#[test]
fn fresh_git_scope_invalidates_unchanged_opencode_rows_and_reports_uncertainty() {
    use std::os::unix::fs::symlink;

    let session = Session::new();
    let alias = session.repo.temp.path().join("session-directory");
    symlink(&session.repo.root, &alias).unwrap();
    session.directory(&alias);
    let bytes = fs::read(&session.source).unwrap();
    let mut compact = BRIEF.to_vec();
    compact.push("--compact");
    let before = session.run(&compact);
    let original = expanded_items(&before).remove(0);

    fs::remove_file(&alias).unwrap();
    symlink(&session.linked, &alias).unwrap();
    let moved = session.run(&compact);
    assert_eq!(fs::read(&session.source).unwrap(), bytes);
    assert_new_empty_view(&before, &moved);
    assert_eq!(
        session.run(&compact)["freshness"]["view_id"],
        moved["freshness"]["view_id"]
    );

    fs::remove_file(&alias).unwrap();
    let unresolved = session.run(&compact);
    assert_eq!(fs::read(&session.source).unwrap(), bytes);
    assert_ne!(
        unresolved["freshness"]["view_id"],
        moved["freshness"]["view_id"]
    );
    assert_eq!(unresolved["freshness"]["status"], "incomplete");
    assert_eq!(unresolved["coverage"]["capture_incomplete"], true);
    assert!(expanded_items(&unresolved).is_empty());
    assert!(
        unresolved["coverage"]["capture"]["harness-opencode"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|error| error["reason"] == "scope_unresolved")
    );
    session.history(&before, &original);

    symlink(&session.repo.root, &alias).unwrap();
    let restored = session.run(&compact);
    assert_eq!(fs::read(&session.source).unwrap(), bytes);
    assert_eq!(restored["freshness"]["status"], "current");
    let items = expanded_items(&restored);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], original["id"]);
    let full = session.run(BRIEF);
    assert_eq!(
        full["freshness"]["view_id"],
        restored["freshness"]["view_id"]
    );
    assert_eq!(
        expanded_items(&full)[0]["observation"]["worktree_key"],
        json!(session.repo.root)
    );
}

fn persisted(session: &Session) -> Value {
    let db = Connection::open(session.repo.db_path()).unwrap();
    let views = db
        .prepare("SELECT view_id,meta_json FROM views ORDER BY view_id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let cursors = db
        .prepare("SELECT source_id,source_path,json FROM cursors ORDER BY source_id,source_path")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    json!({"views":views,"cursors":cursors})
}

fn update_clocks_and_data(session: &Session) -> (i64, i64, i64, String, String) {
    Connection::open(&session.source)
        .unwrap()
        .query_row(
            "SELECT s.time_updated,m.time_updated,p.time_updated,p.data,m.data
             FROM session s JOIN part p ON p.session_id=s.id
             JOIN message m ON m.id=p.message_id",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap()
}

fn mutable_provenance_must_pass_publication_validation(field: &str) {
    let session = Session::new();
    Connection::open(&session.source)
        .unwrap()
        .execute(
            "INSERT INTO message VALUES('message-two','scope-session',1000,?1)",
            [json!({"role":"assistant"}).to_string()],
        )
        .unwrap();
    let before = session.run(BRIEF);
    let original = expanded_items(&before).remove(0);
    let committed = persisted(&session);
    let unchanged = update_clocks_and_data(&session);
    let pause = session.repo.temp.path().join("publication-pause");
    let mut request = BRIEF.to_vec();
    request.extend(["--task", "provenance-validation"]);
    let mut command = session.command();
    command
        .args(&request)
        .env("MEMQ_PAUSE_AT", "before_publish")
        .env("MEMQ_PAUSE_FILE", &pause);
    let mut pauses = 0;
    let published = run_command(&mut command, || {
        if !pause.exists() {
            return;
        }
        assert_eq!(
            persisted(&session),
            committed,
            "failed validation advanced committed views or capture cursors"
        );
        if pauses == 0 {
            match field {
                "directory" => session.directory(&session.linked),
                "message_id" => {
                    Connection::open(&session.source)
                        .unwrap()
                        .execute("UPDATE part SET message_id='message-two'", [])
                        .unwrap();
                }
                "time_created" => {
                    Connection::open(&session.source)
                        .unwrap()
                        .execute("UPDATE part SET time_created=2000", [])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert_eq!(update_clocks_and_data(&session), unchanged);
        }
        pauses += 1;
        fs::remove_file(&pause).unwrap();
    });
    assert_eq!(
        pauses, 2,
        "mutable provenance was not rejected and retried before publication"
    );
    assert_eq!(published["freshness"]["status"], "current");
    assert_eq!(
        session.run(&request)["freshness"]["view_id"],
        published["freshness"]["view_id"],
        "unchanged provenance must reuse its validated view"
    );
    let items = expanded_items(&published);
    if field == "directory" {
        assert!(items.is_empty());
    } else {
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], original["id"]);
        assert_ne!(items[0]["version_id"], original["version_id"]);
        let raw: Value = serde_json::from_str(items[0]["text"].as_str().unwrap()).unwrap();
        if field == "message_id" {
            assert_eq!(raw["message_id"], "message-two");
        } else {
            assert_eq!(raw["timestamp"], "1970-01-01T00:00:02+00:00");
            assert_eq!(items[0]["observation"]["recorded_at"], raw["timestamp"]);
        }
    }
    // Each task owns a separate current-view slot. Refresh the original
    // scope before expecting its previous view to be labeled retained.
    let current = session.run(BRIEF);
    assert_ne!(
        current["freshness"]["view_id"],
        before["freshness"]["view_id"]
    );
    let retained = session.history(&before, &original);
    assert_eq!(retained["version_id"], original["version_id"]);
    assert_eq!(retained["observation"], original["observation"]);
}

#[test]
fn opencode_directory_is_revalidated_before_publication() {
    mutable_provenance_must_pass_publication_validation("directory");
}

#[test]
fn opencode_message_relinking_is_revalidated_before_publication() {
    mutable_provenance_must_pass_publication_validation("message_id");
}

#[test]
fn opencode_recorded_time_is_revalidated_before_publication() {
    mutable_provenance_must_pass_publication_validation("time_created");
}
