mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use support::Repo;
use tempfile::NamedTempFile;

const BRIEF: &[&str] = &["brief", "--budget-kind", "bytes", "--budget", "80000"];
const WAIT_LIMIT: Duration = Duration::from_secs(20);

fn command(r: &Repo, sessions: &Path) -> Command {
    let mut command = r.command();
    command.env("MEMQ_OMP_STORE", sessions);
    for name in ["MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
        command.env(name, "");
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

fn session(r: &Repo, entries: &[(&str, &str)]) -> String {
    let header = json!({
        "type":"session","version":3,"id":"capture-publication","cwd":r.root,
        "git":{"branch":"main"},"timestamp":"2026-09-13T19:00:00Z"
    });
    let mut text = format!("{header}\n");
    for (id, content) in entries {
        let entry = json!({
            "type":"message","id":id,"parentId":null,"timestamp":"2026-09-13T19:01:00Z",
            "message":{"role":"assistant","content":[{"type":"text","text":content}]}
        });
        text.push_str(&format!("{entry}\n"));
    }
    text
}

fn fixture() -> (Repo, PathBuf) {
    let r = Repo::new();
    let sessions = r.temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let initialized = command(&r, &sessions).arg("init").output().unwrap();
    assert!(initialized.status.success());
    fs::write(
        sessions.join("synthetic.jsonl"),
        session(&r, &[("one", "Stable captured evidence")]),
    )
    .unwrap();
    (r, sessions)
}

fn response(out: Output) -> Value {
    assert!(
        out.status.success(),
        "CLI failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn brief(r: &Repo, sessions: &Path, compact: bool) -> Value {
    let mut command = command(r, sessions);
    command.args(BRIEF);
    if compact {
        command.arg("--compact");
    }
    response(command.output().unwrap())
}

// These deadlines bound fixture coordination, not capture performance. Regular
// output files avoid pipe backpressure; Drop reaps a child if an assertion fails.
struct Running {
    child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
}

impl Running {
    fn spawn(command: &mut Command) -> Self {
        let stdout = NamedTempFile::new().unwrap();
        let stderr = NamedTempFile::new().unwrap();
        let child = command
            .stdin(Stdio::piped())
            .stdout(stdout.reopen().unwrap())
            .stderr(stderr.reopen().unwrap())
            .spawn()
            .unwrap();
        Self {
            child,
            stdout,
            stderr,
        }
    }

    fn diagnostics(&self) -> String {
        format!(
            "{}\n{}",
            String::from_utf8_lossy(&fs::read(self.stdout.path()).unwrap()),
            String::from_utf8_lossy(&fs::read(self.stderr.path()).unwrap())
        )
    }

    fn paused(&mut self, path: &Path) {
        let deadline = Instant::now() + WAIT_LIMIT;
        while !path.exists() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "publisher exited before validation pause: {}",
                self.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "publisher did not reach validation pause: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self) -> Output {
        let deadline = Instant::now() + WAIT_LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Output {
                    status,
                    stdout: fs::read(self.stdout.path()).unwrap(),
                    stderr: fs::read(self.stderr.path()).unwrap(),
                };
            }
            assert!(
                Instant::now() < deadline,
                "publisher did not finish: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn mcp_brief(r: &Repo, sessions: &Path) -> Value {
    let mut process = Running::spawn(command(r, sessions).arg("mcp"));
    let mut input = process.child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"brief","arguments":{
            "compact":true,"budget_kind":"bytes","budget":80000
        }}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let out = process.finish();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reply = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap();
    assert!(reply["error"].is_null(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let value = &reply["result"]["structuredContent"];
    support::budget_check(&serde_json::to_vec(value).unwrap());
    value.clone()
}

fn persisted(r: &Repo) -> Value {
    let db = rusqlite::Connection::open_with_flags(
        r.db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let views = db
        .prepare("SELECT view_id,inputs_hash,meta_json,is_current FROM views ORDER BY view_id")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
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
    let versions: i64 = db
        .query_row("SELECT count(*) FROM item_versions", [], |row| row.get(0))
        .unwrap();
    json!({"views":views,"cursors":cursors,"versions":versions})
}

fn assert_offset(r: &Repo, length: usize) {
    let state = persisted(r);
    let cursor: Value = serde_json::from_str(state["cursors"][0][2].as_str().unwrap()).unwrap();
    assert_eq!(cursor["byte_offset"], length);
}

#[test]
fn capture_cache_reuse_keeps_view_identity_coverage_and_occurrences() {
    let (r, sessions) = fixture();
    let first = brief(&r, &sessions, false);
    assert_eq!(first["freshness"]["status"], "current");
    assert_eq!(support::expanded_items(&first).len(), 1);
    let before = persisted(&r);
    let full = brief(&r, &sessions, false);
    let compact = brief(&r, &sessions, true);
    let mcp = mcp_brief(&r, &sessions);
    assert_eq!(
        support::expanded_items(&full),
        support::expanded_items(&first)
    );
    assert_eq!(
        support::expanded_items(&compact),
        support::expanded_items(&mcp)
    );
    assert_eq!(
        support::expanded_items(&compact)[0]["id"],
        support::expanded_items(&first)[0]["id"]
    );
    for again in [&full, &compact, &mcp] {
        assert_eq!(again["freshness"], first["freshness"]);
        assert_eq!(again["coverage"], first["coverage"]);
        assert_eq!(persisted(&r), before);
    }
}

#[test]
fn failed_capture_publication_retains_committed_cursors_and_freshness() {
    let (r, sessions) = fixture();
    let first = brief(&r, &sessions, true);
    let before = persisted(&r);
    let changed = session(
        &r,
        &[
            ("one", "Stable captured evidence"),
            ("two", "Pending captured evidence"),
        ],
    );
    fs::write(sessions.join("synthetic.jsonl"), &changed).unwrap();
    for point in ["after_snapshot", "before_publish"] {
        let out = command(&r, &sessions)
            .env("MEMQ_FAULT", point)
            .args(BRIEF)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86), "fault point: {point}");
        assert_eq!(persisted(&r), before, "fault point: {point}");
    }
    let recovered = brief(&r, &sessions, true);
    assert_eq!(recovered["freshness"]["status"], "current");
    assert_ne!(
        recovered["freshness"]["view_id"],
        first["freshness"]["view_id"]
    );
    assert_eq!(support::expanded_items(&recovered).len(), 2);
    assert_offset(&r, changed.len());
}

#[test]
fn capture_cache_does_not_hide_changes_from_the_publication_guard() {
    let (r, sessions) = fixture();
    let first = brief(&r, &sessions, true);
    let before = persisted(&r);
    let path = sessions.join("synthetic.jsonl");
    fs::write(
        &path,
        session(
            &r,
            &[
                ("one", "Stable captured evidence"),
                ("two", "Pending captured evidence"),
            ],
        ),
    )
    .unwrap();
    let pause = r.temp.path().join("publication-pause");
    let mut publisher = Running::spawn(
        command(&r, &sessions)
            .env("MEMQ_PAUSE_AT", "before_publish")
            .env("MEMQ_PAUSE_FILE", &pause)
            .args(BRIEF)
            .arg("--compact"),
    );
    publisher.paused(&pause);
    assert_eq!(persisted(&r), before);
    let latest = session(
        &r,
        &[
            ("one", "Stable captured evidence"),
            ("two", "Pending captured evidence"),
            ("three", "Changed during validation"),
        ],
    );
    fs::write(path, &latest).unwrap();
    fs::remove_file(&pause).unwrap();

    // A changed capture inventory must roll back the first transaction and
    // retry. At the retry's pause, only the original view/cursor is committed.
    publisher.paused(&pause);
    assert_eq!(persisted(&r), before);
    fs::remove_file(&pause).unwrap();
    let published = response(publisher.finish());
    assert_eq!(published["freshness"]["status"], "current");
    assert_ne!(
        published["freshness"]["view_id"],
        first["freshness"]["view_id"]
    );
    let items = support::expanded_items(&published);
    assert_eq!(items.len(), 3);
    assert!(items.iter().any(|item| {
        item["text"]
            .as_str()
            .unwrap()
            .contains("Changed during validation")
    }));
    assert_offset(&r, latest.len());
}
