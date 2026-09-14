mod support;

use memq::core::ReadRequest;
use serde_json::{Value, json};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};
use support::{Repo, expanded_items};

const LIMIT: usize = 80000;

fn command(r: &Repo) -> Command {
    let mut command = r.command();
    for variable in ["MEMQ_OMP_STORE", "MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
        command.env(variable, "");
    }
    for variable in [
        "MEMQ_CODE_REPORT",
        "MEMQ_FAULT",
        "MEMQ_PAUSE_AT",
        "MEMQ_PAUSE_FILE",
    ] {
        command.env_remove(variable);
    }
    command
}

fn checked(raw: &[u8]) -> Value {
    support::budget_check(raw);
    serde_json::from_slice(raw).unwrap()
}

fn ok(r: &Repo, args: &[&str]) -> Value {
    let out = command(r).args(args).output().unwrap();
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

fn records_fixture() -> (Repo, Value) {
    let r = initialized();
    r.record_source();
    r.records(json!([
        {"id":"first","text":"Synthetic orchard first evidence"},
        {"id":"second","text":"Synthetic orchard second evidence"}
    ]));
    r.commit("synthetic availability sources");
    let view = ok(
        &r,
        &["brief", "--budget-kind", "bytes", "--budget", "80000"],
    );
    (r, view)
}

fn request(view: &Value, compact: bool) -> ReadRequest {
    ReadRequest {
        view_id: Some(view["freshness"]["view_id"].as_str().unwrap().to_owned()),
        compact,
        incoming: Some(true),
        query: Some("orchard".into()),
        ids: expanded_items(view)
            .iter()
            .map(|item| item["id"].as_str().unwrap().to_owned())
            .collect(),
        budget_kind: Some("bytes".into()),
        budget: Some(LIMIT),
        ..ReadRequest::default()
    }
}

fn read_cli(r: &Repo, operation: &str, request: &ReadRequest) -> Value {
    let mut cmd = command(r);
    cmd.arg(operation);
    if operation == "search" {
        cmd.arg(request.query.as_deref().unwrap());
    } else if operation == "show" {
        cmd.args(&request.ids);
    }
    cmd.args(["--view-id", request.view_id.as_deref().unwrap()]);
    cmd.args([
        "--incoming",
        "true",
        "--budget-kind",
        "bytes",
        "--budget",
        "80000",
    ]);
    if request.compact {
        cmd.arg("--compact");
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    checked(&out.stdout)
}

fn assert_same_evidence(response: &Value, original: &Value, availability: &str) {
    let actual = expanded_items(response);
    let original = expanded_items(original);
    assert_eq!(actual.len(), original.len());
    for item in actual {
        let before = original
            .iter()
            .find(|before| before["id"] == item["id"])
            .unwrap();
        assert_eq!(item["availability"], availability);
        assert_eq!(item["pointer"], before["pointer"]);
        assert_eq!(item["text"], before["text"]);
    }
}

#[test]
fn cli_retained_reads_refresh_availability_when_a_source_disappears_and_returns() {
    let (r, view) = records_fixture();
    let path = r.root.join("records.json");
    let source = fs::read(&path).unwrap();
    for availability in ["current", "source_missing", "current"] {
        if availability == "source_missing" {
            fs::remove_file(&path).unwrap();
        } else {
            fs::write(&path, &source).unwrap();
        }
        for compact in [false, true] {
            for operation in ["brief", "search", "show"] {
                let page = read_cli(&r, operation, &request(&view, compact));
                assert_same_evidence(&page, &view, availability);
            }
        }
    }
}

struct Mcp {
    child: Child,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn new(r: &Repo, text: bool) -> Self {
        let mut cmd = command(r);
        cmd.arg("mcp");
        if text {
            cmd.arg("--text-fallback");
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdout,
            next_id: 1,
        }
    }

    fn read(&mut self, operation: &str, request: &ReadRequest) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":operation,"arguments":request}
            })
        )
        .unwrap();
        let mut line = String::new();
        assert!(self.stdout.read_line(&mut line).unwrap() > 0);
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], id);
        assert!(response["error"].is_null(), "{response}");
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{response}");
        if let Some(text) = result["content"][0]["text"].as_str() {
            assert!(result.get("structuredContent").is_none());
            checked(text.as_bytes())
        } else {
            assert_eq!(result["content"], json!([]));
            checked(&serde_json::to_vec(&result["structuredContent"]).unwrap())
        }
    }

    fn finish(mut self) {
        self.child.stdin.take();
        assert!(self.child.wait().unwrap().success());
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn one_mcp_connection_rechecks_availability_in_both_transports() {
    for text in [false, true] {
        let (r, view) = records_fixture();
        let path = r.root.join("records.json");
        let source = fs::read(&path).unwrap();
        let mut server = Mcp::new(&r, text);
        for availability in ["current", "source_missing", "current"] {
            if availability == "source_missing" {
                fs::remove_file(&path).unwrap();
            } else {
                fs::write(&path, &source).unwrap();
            }
            for compact in [false, true] {
                for operation in ["brief", "search", "show"] {
                    let page = server.read(operation, &request(&view, compact));
                    assert_same_evidence(&page, &view, availability);
                }
            }
        }
        server.finish();
    }
}

fn journal(r: &Repo, kind: &str) -> String {
    if kind == "omp" {
        format!(
            "{}\n{}\n",
            json!({"type":"session","version":3,"id":"synthetic-availability-omp","cwd":r.root}),
            json!({"type":"message","id":"entry","parentId":null,"message":{
                "role":"assistant","content":[{"type":"text","text":"Synthetic orchard OMP evidence"}]
            }})
        )
    } else {
        format!(
            "{}\n{}\n",
            json!({"type":"session_meta","ordinal":0,"payload":{
                "id":"synthetic-availability-codex","cwd":r.root,"cli_version":"0.154.0","git":{"branch":"main"}
            }}),
            json!({"type":"response_item","ordinal":1,"payload":{
                "type":"message","role":"assistant","content":[{"type":"output_text","text":"Synthetic orchard Codex evidence"}]
            }})
        )
    }
}

fn captured_view(r: &Repo, omp: &Path, codex: Option<&Path>) -> Value {
    let mut cmd = command(r);
    cmd.env("MEMQ_OMP_STORE", omp);
    if let Some(codex) = codex {
        cmd.env("MEMQ_CODEX_STORE", codex);
    }
    let out = cmd
        .args(["brief", "--budget-kind", "bytes", "--budget", "80000"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    checked(&out.stdout)
}

#[test]
fn identical_relative_paths_keep_repository_and_distinct_harness_roots_separate() {
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='shared.jsonl'\n");
    r.write("shared.jsonl", "Synthetic orchard repository evidence\n");
    let omp = r.temp.path().join("omp");
    let codex = r.temp.path().join("codex");
    fs::create_dir(&omp).unwrap();
    fs::create_dir(&codex).unwrap();
    let omp_source = journal(&r, "omp");
    let codex_source = journal(&r, "codex");
    fs::write(omp.join("shared.jsonl"), &omp_source).unwrap();
    fs::write(codex.join("shared.jsonl"), &codex_source).unwrap();
    let view = captured_view(&r, &omp, Some(&codex));
    assert_eq!(expanded_items(&view).len(), 3);
    for missing in ["harness-omp", "harness-codex", "repository"] {
        fs::write(omp.join("shared.jsonl"), &omp_source).unwrap();
        fs::write(codex.join("shared.jsonl"), &codex_source).unwrap();
        r.write("shared.jsonl", "Synthetic orchard repository evidence\n");
        let path = match missing {
            "harness-omp" => omp.join("shared.jsonl"),
            "harness-codex" => codex.join("shared.jsonl"),
            _ => r.root.join("shared.jsonl"),
        };
        fs::remove_file(path).unwrap();
        for compact in [false, true] {
            let page = read_cli(&r, "show", &request(&view, compact));
            let items = expanded_items(&page);
            assert_eq!(items.len(), 3);
            for item in items {
                let kind = item["kind"].as_str().unwrap();
                let absent =
                    kind == missing || (missing == "repository" && !kind.starts_with("harness-"));
                assert_eq!(
                    item["availability"],
                    if absent {
                        "source_missing"
                    } else if kind.starts_with("harness-") {
                        "historical"
                    } else {
                        "current"
                    }
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn repository_read_checks_and_harness_presence_do_not_share_a_cache_entry() {
    use std::os::unix::fs::symlink;
    let r = initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='shared.jsonl'\n");
    let source = journal(&r, "omp");
    r.write("shared.jsonl", &source);
    let canonical_root = fs::canonicalize(&r.root).unwrap();
    let view = captured_view(&r, &canonical_root, None);
    assert_eq!(expanded_items(&view).len(), 2);
    let outside = r.temp.path().join("outside.jsonl");
    fs::write(&outside, &source).unwrap();
    fs::remove_file(r.root.join("shared.jsonl")).unwrap();
    symlink(outside, r.root.join("shared.jsonl")).unwrap();
    for compact in [false, true] {
        let page = read_cli(&r, "show", &request(&view, compact));
        for item in expanded_items(&page) {
            assert_eq!(
                item["availability"],
                if item["kind"] == "harness-omp" {
                    "historical"
                } else {
                    "source_missing"
                }
            );
        }
    }
}

#[test]
fn shared_source_presence_keeps_current_historical_and_forgotten_classification_per_item() {
    let (r, before) = records_fixture();
    let original = expanded_items(&before);
    let removed = original
        .iter()
        .find(|item| item["native_id"] == "first")
        .unwrap();
    r.records(json!([{"id":"second","text":"Synthetic orchard second evidence"}]));
    r.commit("synthetic record retirement");
    let view = ok(
        &r,
        &["brief", "--budget-kind", "bytes", "--budget", "80000"],
    );
    let path = r.root.join("records.json");
    let source = fs::read(&path).unwrap();
    let mut request = request(&before, false);
    request.view_id = Some(view["freshness"]["view_id"].as_str().unwrap().to_owned());
    for compact in [false, true] {
        request.compact = compact;
        let shown = read_cli(&r, "show", &request);
        for item in expanded_items(&shown) {
            if item["id"] == removed["id"] {
                assert_eq!(item["availability"], "historical");
                assert_eq!(item["text"], removed["text"]);
                assert_eq!(item["evidence_view_id"], before["freshness"]["view_id"]);
            } else {
                assert_eq!(item["availability"], "current");
            }
        }
    }
    fs::remove_file(&path).unwrap();
    let missing = read_cli(&r, "show", &request);
    assert!(
        expanded_items(&missing)
            .iter()
            .all(|item| item["availability"] == "source_missing")
    );
    ok(&r, &["forget", removed["id"].as_str().unwrap()]);
    for restore in [false, true] {
        if restore {
            fs::write(&path, &source).unwrap();
        }
        let page = read_cli(&r, "show", &request);
        for item in expanded_items(&page) {
            if item["id"] == removed["id"] {
                assert_eq!(item, json!({"id":removed["id"],"availability":"forgotten"}));
            } else {
                assert_eq!(
                    item["availability"],
                    if restore { "current" } else { "source_missing" }
                );
            }
        }
    }
}

#[test]
fn incoming_members_keep_their_availability_when_the_local_source_disappears() {
    let (r, _) = records_fixture();
    let side = r.linked("side");
    fs::write(
        side.join("records.json"),
        serde_json::to_vec(&json!({"records":[
            {"id":"first","text":"Synthetic orchard incoming first evidence"},
            {"id":"second","text":"Synthetic orchard incoming second evidence"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    support::git(&side, &["add", "records.json"]);
    support::git(
        &side,
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "synthetic incoming evidence",
        ],
    );
    let remote = r.bare_remote();
    support::git(
        &remote,
        &[
            "fetch",
            "-q",
            side.to_str().unwrap(),
            "HEAD:refs/heads/main",
        ],
    );
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    r.add_source("[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=2\nmin_interval_seconds=300\n");
    let view = ok(
        &r,
        &["brief", "--budget-kind", "bytes", "--budget", "80000"],
    );
    assert_eq!(expanded_items(&view).len(), 4);
    fs::remove_file(r.root.join("records.json")).unwrap();
    for compact in [false, true] {
        let mut request = request(&view, compact);
        request.ids.sort();
        request.ids.dedup();
        for operation in ["brief", "search", "show"] {
            let page = read_cli(&r, operation, &request);
            let items = expanded_items(&page);
            assert_eq!(items.len(), 4);
            assert_eq!(
                items
                    .iter()
                    .filter(|item| item["availability"] == "incoming")
                    .count(),
                2
            );
            for item in items {
                assert_eq!(
                    item["availability"],
                    if item["observation"]["origin"] == "incoming" {
                        "incoming"
                    } else {
                        "source_missing"
                    }
                );
            }
        }
    }
}
