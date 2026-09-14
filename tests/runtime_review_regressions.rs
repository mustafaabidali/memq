mod support;

use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use support::Repo;
use tempfile::NamedTempFile;

const TIMEOUT: Duration = Duration::from_secs(8);
const BRIEF: &[&str] = &[
    "brief",
    "--compact",
    "--budget-kind",
    "bytes",
    "--budget",
    "80000",
];

fn command(r: &Repo) -> Command {
    let mut command = r.command();
    for name in ["MEMQ_OMP_STORE", "MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
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

// Regular output files avoid pipe backpressure. Every wait has a deadline,
// including MCP responses while stdin stays open; Drop reaps failed fixtures.
struct Process {
    child: Child,
    stdout: NamedTempFile,
    stderr: NamedTempFile,
    consumed: usize,
}

impl Process {
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
            consumed: 0,
        }
    }

    fn diagnostics(&self) -> String {
        format!(
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&fs::read(self.stdout.path()).unwrap()),
            String::from_utf8_lossy(&fs::read(self.stderr.path()).unwrap())
        )
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let bytes = fs::read(self.stdout.path()).unwrap();
            if let Some(end) = bytes[self.consumed..].iter().position(|b| *b == b'\n') {
                let end = self.consumed + end + 1;
                let response: Value = serde_json::from_slice(&bytes[self.consumed..end]).unwrap();
                self.consumed = end;
                assert_eq!(response["jsonrpc"], "2.0", "{}", self.diagnostics());
                assert_eq!(response["id"], id);
                assert!(response["error"].is_null(), "{response}");
                return response;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "MCP exited before responding: {}",
                self.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "MCP response timed out: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handshake(&mut self) {
        let initialized = self.request(1, "initialize", json!({"protocolVersion":"2025-06-18"}));
        assert_eq!(initialized["result"]["serverInfo"]["name"], "memq");
        let tools = self.request(2, "tools/list", json!({}));
        let names: Vec<_> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["brief", "search", "show", "note"]);
    }

    fn brief(&mut self, id: u64, error: Option<&str>) -> Value {
        let response = self.request(
            id,
            "tools/call",
            json!({"name":"brief","arguments":{"compact":true,"budget_kind":"bytes","budget":80000}}),
        );
        assert_eq!(response["result"]["isError"], error.is_some(), "{response}");
        let result = &response["result"];
        let raw = if let Some(text) = result["content"][0]["text"].as_str() {
            assert!(result.get("structuredContent").is_none());
            text.as_bytes().to_owned()
        } else {
            assert_eq!(result["content"], json!([]));
            serde_json::to_vec(&result["structuredContent"]).unwrap()
        };
        let payload: Value = serde_json::from_slice(&raw).unwrap();
        if let Some(code) = error {
            assert_eq!(payload["error"]["code"], code, "{payload}");
        } else {
            support::budget_check(&raw);
        }
        payload
    }

    fn wait_for_file(&mut self, path: &Path) {
        let deadline = Instant::now() + TIMEOUT;
        while !path.exists() {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "{}",
                self.diagnostics()
            );
            assert!(Instant::now() < deadline, "fixture pause timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(mut self) -> Output {
        self.child.stdin.take();
        let deadline = Instant::now() + TIMEOUT;
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
                "process timed out: {}",
                self.diagnostics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run(r: &Repo, args: &[&str]) -> Output {
    Process::spawn(command(r).args(args)).finish()
}

fn ok(r: &Repo, args: &[&str]) -> Value {
    let out = run(r, args);
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    if value.get("budget").is_some() {
        support::budget_check(&out.stdout);
    }
    value
}

fn initialized() -> Repo {
    let r = Repo::new();
    ok(&r, &["init"]);
    r
}

fn mcp(r: &Repo, text: bool) -> Process {
    let mut cmd = command(r);
    cmd.arg("mcp");
    if text {
        cmd.arg("--text-fallback");
    }
    Process::spawn(&mut cmd)
}

fn finish_mcp(server: Process) -> Output {
    let out = server.finish();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in out
        .stdout
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
    {
        let frame: Value = serde_json::from_slice(line).unwrap();
        assert_eq!(
            frame["jsonrpc"], "2.0",
            "stdout must contain only JSON-RPC frames"
        );
    }
    out
}

fn startup_lock_recovery(text: bool) {
    let r = initialized();
    let ready = r.temp.path().join("writer-ready");
    let mut writer = Process::spawn(
        command(&r)
            .env("MEMQ_PAUSE_AT", "after_note_reservation")
            .env("MEMQ_PAUSE_FILE", &ready)
            .args([
                "note",
                "--text",
                "Synthetic recovered writer",
                "--idempotency-key",
                "runtime-writer",
            ]),
    );
    writer.wait_for_file(&ready);
    let mut server = mcp(&r, text);
    server.handshake();
    server.brief(3, Some("in_progress"));
    fs::remove_file(&ready).unwrap();
    let written = writer.finish();
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stdout)
    );
    let note: Value = serde_json::from_slice(&written.stdout).unwrap();
    let recovered = server.brief(4, None);
    assert_eq!(recovered["freshness"]["status"], "current");
    assert!(
        support::expanded_items(&recovered)
            .iter()
            .any(|item| item["id"] == note["id"])
    );
    let cli = ok(&r, BRIEF);
    assert_eq!(
        support::expanded_items(&recovered),
        support::expanded_items(&cli)
    );
    let out = finish_mcp(server);
    assert!(String::from_utf8_lossy(&out.stderr).contains("in_progress"));
}

#[test]
fn structured_mcp_survives_startup_lock_and_retries_after_release() {
    startup_lock_recovery(false);
}

#[test]
fn text_mcp_survives_startup_lock_and_retries_after_release() {
    startup_lock_recovery(true);
}

#[test]
fn mcp_retries_startup_reconciliation_after_source_identity_is_restored() {
    let r = initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"Synthetic retained source"}]));
    ok(&r, BRIEF);
    let path = r.root.join(".memq/config.toml");
    let config = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        config.replace("id = \"records\"", "id = \"renamed\""),
    )
    .unwrap();
    let mut server = mcp(&r, false);
    server.handshake();
    server.brief(3, Some("source_identity_missing"));
    fs::write(path, config).unwrap();
    let recovered = server.brief(4, None);
    assert_eq!(recovered["freshness"]["status"], "current");
    assert_eq!(support::expanded_items(&recovered).len(), 1);
    let out = finish_mcp(server);
    assert!(String::from_utf8_lossy(&out.stderr).contains("source_identity_missing"));
}

fn assert_core_error(r: &Repo, code: &str) {
    let cli = run(r, BRIEF);
    assert!(!cli.status.success());
    let cli: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(cli["error"]["code"], code);
    let mut server = mcp(r, false);
    server.handshake();
    for id in [3, 4] {
        let payload = server.brief(id, Some(code));
        assert_eq!(payload["error"], cli["error"]);
    }
    let out = finish_mcp(server);
    assert!(String::from_utf8_lossy(&out.stderr).contains(code));
}

#[test]
fn mcp_remains_git_only_when_startup_cannot_discover_a_repository() {
    let mut r = initialized();
    r.root = r.temp.path().join("outside");
    fs::create_dir(&r.root).unwrap();
    assert_core_error(&r, "not_a_git_repository");
}

#[test]
fn mcp_does_not_bypass_newer_schema_after_startup_failure() {
    let r = initialized();
    let path = r.db_path();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=999").unwrap();
    drop(db);
    assert_core_error(&r, "unsupported_schema");
    let db = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        999
    );
}

#[test]
fn mcp_does_not_bypass_damaged_deletion_ledger_after_startup_failure() {
    let r = initialized();
    let db = rusqlite::Connection::open(r.store_root().join("access.sqlite")).unwrap();
    db.execute_batch("DROP TABLE tombstones").unwrap();
    drop(db);
    assert_core_error(&r, "access_state_incomplete");
}

#[test]
fn mcp_does_not_bypass_invalid_tombstone_after_startup_failure() {
    let r = initialized();
    r.write(".memq/tombstones/invalid.json", "{}");
    assert_core_error(&r, "invalid_tombstone");
}

#[test]
fn repeat_init_reports_existing_config_and_preserves_identity_and_bytes() {
    let r = initialized();
    let path = r.root.join(".memq/config.toml");
    let original = fs::read_to_string(&path).unwrap();
    let original_id = memq::config::Config::parse(&original).unwrap().project_id;
    let configured =
        original.replace("budget = 4000", "budget = 6000") + "\n# Synthetic customization\n";
    fs::write(&path, &configured).unwrap();
    for content in [
        configured.as_str(),
        "format = 99\n# Preserve unsupported config too\n",
    ] {
        fs::write(&path, content).unwrap();
        let out = run(&r, &["init"]);
        assert_eq!(out.status.code(), Some(2));
        let error: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(error["error"]["code"], "invalid_config");
        assert!(
            error["error"]["detail"]
                .as_str()
                .unwrap()
                .contains("already initialized")
        );
        assert_eq!(fs::read(&path).unwrap(), content.as_bytes());
    }
    fs::write(path, configured).unwrap();
    assert_eq!(ok(&r, BRIEF)["scope"]["project_id"], original_id);
}

#[cfg(unix)]
fn fifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // Only an ordinary synthetic named pipe inside this test's temporary directory.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
}

#[cfg(unix)]
#[test]
fn fifo_source_returns_bounded_diagnostics_and_mcp_recovers_after_replacement() {
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    let path = r.root.join("guide.md");
    fifo(&path);
    let cli = run(&r, BRIEF);
    assert!(!cli.status.success());
    let error: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(error["error"]["code"], "recovery_limit_reached");
    assert_eq!(
        error["error"]["detail"]["coverage"]["sources_missing"][0]["reason"],
        "source_not_regular"
    );
    let mut server = mcp(&r, false);
    server.handshake();
    let failed = server.brief(3, Some("recovery_limit_reached"));
    assert_eq!(failed["error"], error["error"]);
    fs::remove_file(path).unwrap();
    r.write("guide.md", "Synthetic restored regular source\n");
    let restored = server.brief(4, None);
    assert_eq!(restored["freshness"]["status"], "current");
    assert!(support::expanded_items(&restored).iter().any(|item| {
        item["text"]
            .as_str()
            .unwrap()
            .contains("Synthetic restored regular source")
    }));
    finish_mcp(server);
}

#[cfg(unix)]
#[test]
fn fifo_config_fails_promptly_without_changing_or_recreating_it() {
    let r = initialized();
    let path = r.root.join(".memq/config.toml");
    fs::remove_file(&path).unwrap();
    fifo(&path);
    let out = run(&r, BRIEF);
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_config");
    assert!(
        error["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("regular file")
    );
    assert!(!fs::symlink_metadata(path).unwrap().is_file());
}

#[cfg(unix)]
#[test]
fn dirty_fifo_target_is_skipped_with_diagnostics_and_regular_bytes_still_change_views() {
    use std::os::unix::fs::symlink;
    let r = initialized();
    fifo(&r.root.join("stream"));
    symlink("stream", r.root.join("untracked.txt")).unwrap();
    let out = run(&r, BRIEF);
    assert!(out.status.success());
    support::budget_check(&out.stdout);
    assert!(String::from_utf8_lossy(&out.stderr).contains("source_not_regular"));
    let before: Value = serde_json::from_slice(&out.stdout).unwrap();
    fs::remove_file(r.root.join("untracked.txt")).unwrap();
    r.write("untracked.txt", "Synthetic first regular bytes");
    let regular = ok(&r, BRIEF);
    assert_ne!(
        before["freshness"]["inputs_hash"],
        regular["freshness"]["inputs_hash"]
    );
    r.write("untracked.txt", "Synthetic other regular bytes");
    let changed = ok(&r, BRIEF);
    assert_ne!(
        regular["freshness"]["inputs_hash"],
        changed["freshness"]["inputs_hash"]
    );
    assert!(
        changed["changes_since"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path == "untracked.txt")
    );
}

#[cfg(unix)]
#[test]
fn fifo_in_authored_source_directories_is_never_silently_omitted() {
    for dir in [".memq/notes", ".memq/tombstones"] {
        let r = initialized();
        fs::create_dir_all(r.root.join(dir)).unwrap();
        fifo(&r.root.join(dir).join("blocked.json"));
        assert_core_error(&r, "source_not_regular");
    }
}

#[cfg(unix)]
#[test]
fn socket_source_is_refused_without_reading() {
    use std::os::unix::net::UnixListener;
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    let _socket = UnixListener::bind(r.root.join("guide.md")).unwrap();
    let out = run(&r, BRIEF);
    assert!(!out.status.success());
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], "recovery_limit_reached");
    assert_eq!(
        error["error"]["detail"]["coverage"]["sources_missing"][0]["reason"],
        "source_not_regular"
    );
}

#[cfg(unix)]
#[test]
fn fifo_replacement_during_publication_keeps_previous_evidence_stale() {
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.write("guide.md", "Synthetic previously published evidence\n");
    let before = ok(&r, BRIEF);
    r.write("guide.md", "Synthetic candidate that must not publish\n");
    let ready = r.temp.path().join("publication-ready");
    let mut publishing = Process::spawn(
        command(&r)
            .env("MEMQ_PAUSE_AT", "before_publish")
            .env("MEMQ_PAUSE_FILE", &ready)
            .args(BRIEF),
    );
    publishing.wait_for_file(&ready);
    fs::remove_file(r.root.join("guide.md")).unwrap();
    fifo(&r.root.join("guide.md"));
    fs::remove_file(ready).unwrap();
    let out = publishing.finish();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    support::budget_check(&out.stdout);
    let after: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(after["freshness"]["status"], "stale");
    assert_eq!(
        after["freshness"]["view_id"],
        before["freshness"]["view_id"]
    );
    assert_eq!(
        after["coverage"]["sources_missing"][0]["reason"],
        "source_not_regular"
    );
    assert!(support::expanded_items(&after).iter().all(|item| {
        !item["text"]
            .as_str()
            .unwrap()
            .contains("candidate that must not publish")
    }));
}

#[cfg(unix)]
#[test]
fn regular_source_symlinks_work_and_worktree_escape_is_still_refused() {
    use std::os::unix::fs::symlink;
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.write("regular.md", "Synthetic in-worktree evidence\n");
    symlink("regular.md", r.root.join("guide.md")).unwrap();
    let before = ok(&r, BRIEF);
    assert_eq!(before["freshness"]["status"], "current");
    assert_eq!(support::expanded_items(&before).len(), 1);
    let outside = r.temp.path().join("outside.md");
    fs::write(&outside, "Synthetic outside-worktree evidence\n").unwrap();
    fs::remove_file(r.root.join("guide.md")).unwrap();
    symlink(outside, r.root.join("guide.md")).unwrap();
    let refused = ok(&r, BRIEF);
    assert_eq!(refused["freshness"]["status"], "stale");
    assert_eq!(
        refused["coverage"]["sources_missing"][0]["reason"],
        "source_unreadable"
    );
    assert!(support::expanded_items(&refused).iter().all(|item| {
        !item["text"]
            .as_str()
            .unwrap()
            .contains("Synthetic outside-worktree evidence")
    }));
}

#[test]
fn simultaneous_init_has_one_winner_and_preserves_its_project_identity() {
    let r = Repo::new();
    let first = Process::spawn(command(&r).arg("init"));
    let second = Process::spawn(command(&r).arg("init"));
    let (winner, loser) = match (first.finish(), second.finish()) {
        (first, second) if first.status.success() => (first, second),
        (first, second) => (second, first),
    };
    assert!(
        winner.status.success(),
        "{}",
        String::from_utf8_lossy(&winner.stdout)
    );
    assert_eq!(loser.status.code(), Some(2));
    let loser: Value = serde_json::from_slice(&loser.stdout).unwrap();
    assert_eq!(loser["error"]["code"], "invalid_config");
    assert!(
        loser["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("already initialized")
    );
    let winner: Value = serde_json::from_slice(&winner.stdout).unwrap();
    let original = fs::read(r.root.join(".memq/config.toml")).unwrap();
    assert_eq!(ok(&r, BRIEF)["scope"]["project_id"], winner["project_id"]);
    assert_eq!(
        fs::read(r.root.join(".memq/config.toml")).unwrap(),
        original
    );
}

#[cfg(unix)]
#[test]
fn repeat_init_refuses_a_config_symlink_and_preserves_its_target() {
    use std::os::unix::fs::symlink;
    let r = initialized();
    let config = r.root.join(".memq/config.toml");
    let target = r.root.join("config-copy.toml");
    fs::rename(&config, &target).unwrap();
    let original = fs::read(&target).unwrap();
    symlink("../config-copy.toml", &config).unwrap();
    let out = run(&r, &["init"]);
    assert_eq!(out.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(error["error"]["code"], "invalid_config");
    assert!(
        error["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("already initialized")
    );
    assert!(fs::symlink_metadata(&config).unwrap().is_symlink());
    assert_eq!(fs::read(target).unwrap(), original);
    ok(&r, BRIEF);
}
