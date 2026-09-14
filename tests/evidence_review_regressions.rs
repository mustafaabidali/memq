mod support;

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use support::Repo;

fn command(r: &Repo, report: Option<&Path>) -> Command {
    let mut command = r.command();
    for variable in ["MEMQ_OMP_STORE", "MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
        command.env(variable, "");
    }
    command.env_remove("MEMQ_CODE_REPORT");
    if let Some(report) = report {
        command.env("MEMQ_CODE_REPORT", report);
    }
    command
}

fn note(r: &Repo, text: &str, key: &str) -> Value {
    let out = command(r, None)
        .args(["note", "--text", text, "--idempotency-key", key])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "note failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn cli(r: &Repo, args: &[&str], report: Option<&Path>) -> Value {
    let out = command(r, report)
        .args(args)
        .args(["--budget-kind", "bytes", "--budget", "80000"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CLI failed: {}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn mcp(r: &Repo, tool: &str, mut arguments: Value, report: Option<&Path>) -> Value {
    arguments["budget_kind"] = json!("bytes");
    arguments["budget"] = json!(80000);
    let mut child = command(r, report)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":arguments}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "MCP failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let response = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|response| response["id"] == 2)
        .unwrap();
    assert!(response["error"].is_null(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    support::budget_check(&serde_json::to_vec(&response["result"]["structuredContent"]).unwrap());
    response["result"]["structuredContent"].clone()
}

fn assert_working_evidence(item: &Value, path: &str, bytes: &[u8]) {
    assert_eq!(item["committed"], false, "uncommitted bytes at {path}");
    assert_eq!(item["observation"]["dirty"], true);
    assert!(
        item["pointer"].as_str().unwrap().starts_with(&format!(
            "worktree:{path}?sha256={}",
            memq::util::hash(bytes)
        )),
        "working evidence must retain a usable path and its byte hash"
    );
}

#[test]
fn ignored_authored_notes_and_configured_sources_remain_working_evidence() {
    let r = Repo::initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.write(".gitignore", ".memq/notes/\nguide.md\n");
    r.commit("synthetic ignored source configuration");
    r.write("guide.md", "Synthetic ignored guide\n");
    let note = note(&r, "Synthetic ignored note", "ignored-evidence");
    assert_eq!(note["staging"], "ignored");
    let note_path = note["path"].as_str().unwrap();
    let note_bytes = std::fs::read(r.root.join(note_path)).unwrap();
    for path in [note_path, "guide.md"] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&r.root)
            .args(["cat-file", "-e", &format!("HEAD:{path}")])
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "fixture unexpectedly committed {path}"
        );
    }
    assert!(r.git(&["status", "--porcelain"]).is_empty());
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        let responses = [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ];
        assert_eq!(responses[0], responses[1]);
        for response in responses {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 2);
            let authored = items.iter().find(|item| item["id"] == note["id"]).unwrap();
            assert_eq!(authored["text"], "Synthetic ignored note");
            assert_working_evidence(authored, note_path, &note_bytes);
            let guide = items.iter().find(|item| item["id"] != note["id"]).unwrap();
            assert_eq!(guide["text"], "Synthetic ignored guide\n");
            assert_working_evidence(guide, "guide.md", b"Synthetic ignored guide\n");
            if !compact {
                assert!(authored["observation"]["blob_oid"].is_null());
                assert!(guide["observation"]["blob_oid"].is_null());
            }
        }
    }
}

#[test]
fn untracked_authored_notes_and_configured_sources_remain_working_evidence() {
    let r = Repo::initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.commit("synthetic source configuration");
    r.write("guide.md", "Synthetic untracked guide\n");
    let note = note(&r, "Synthetic untracked note", "untracked-evidence");
    let note_path = note["path"].as_str().unwrap();
    r.git(&["restore", "--staged", "--", note_path]);
    let note_bytes = std::fs::read(r.root.join(note_path)).unwrap();
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 2);
            for item in &items {
                if item["id"] == note["id"] {
                    assert_working_evidence(item, note_path, &note_bytes);
                } else {
                    assert_working_evidence(item, "guide.md", b"Synthetic untracked guide\n");
                }
            }
        }
    }
}

#[test]
fn a_clean_status_requires_matching_committed_blob_bytes() {
    let r = Repo::initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.write("guide.md", "Synthetic committed guide\n");
    r.commit("synthetic committed evidence");
    let revision = r.git(&["rev-parse", "HEAD"]);
    let blob = r.git(&["rev-parse", "HEAD:guide.md"]);
    r.git(&["update-index", "--assume-unchanged", "guide.md"]);
    r.write("guide.md", "Synthetic hidden working change\n");
    assert!(r.git(&["status", "--porcelain"]).is_empty());
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["text"], "Synthetic hidden working change\n");
            assert_working_evidence(&items[0], "guide.md", b"Synthetic hidden working change\n");
        }
    }
    r.write("guide.md", "Synthetic committed guide\n");
    let response = cli(&r, &["brief"], None);
    let item = &support::expanded_items(&response)[0];
    assert_eq!(item["committed"], true);
    assert_eq!(item["observation"]["dirty"], false);
    assert_eq!(item["observation"]["blob_oid"], blob);
    assert_eq!(item["observation"]["commit"], revision);
    assert!(
        item["pointer"]
            .as_str()
            .unwrap()
            .contains(&format!("@sha1:{revision}:guide.md"))
    );
}

fn code_scope_fixture() -> (Repo, PathBuf, String) {
    let r = Repo::initialized();
    r.add_source("[[source]]\nid='guide'\nkind='markdown'\npath='guide.md'\n");
    r.write("guide.md", "Synthetic local guide\n");
    r.write("checked.rs", "fn local_only() {}\n");
    r.commit("synthetic local code evidence");
    r.git(&["switch", "-q", "-c", "feature"]);
    r.write("guide.md", "Synthetic feature guide\n");
    r.write("checked.rs", "fn feature_only() {}\n");
    r.commit("synthetic different branch evidence");
    r.git(&["switch", "-q", "main"]);
    let revision = r.git(&["rev-parse", "HEAD"]);
    let project = cli(&r, &["brief"], None)["scope"]["project_id"].clone();
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let report = json!({
        "format":1,"tool":"codebase-memory-mcp","tool_version":"synthetic-v1",
        "report_id":"synthetic-local-code","project_id":project,
        "worktree":memq::util::hash(repo.root.to_string_lossy().as_bytes()),
        "checked_revision":revision,"object_format":"sha1","complete":true,
        "files":[
            {"path":"guide.md","content_sha256":memq::util::hash(b"Synthetic local guide\n")},
            {"path":"checked.rs","content_sha256":memq::util::hash(b"fn local_only() {}\n")}
        ],
        "relations":[{"from":"guide.md","to":"checked.rs::local_only","relationship":"implements"}]
    });
    let path = r.temp.path().join("code-report.json");
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    (r, path, revision)
}

fn code_relation(item: &Value) -> &Value {
    item["relations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|relation| relation["kind"] == "code")
        .expect("the reported relation must retain its provenance")
}

fn assert_code_provenance(relation: &Value, revision: &str) {
    assert_eq!(relation["checked_revision"], revision);
    assert_eq!(relation["coverage_report_id"], "synthetic-local-code");
    assert_eq!(relation["tool"], "codebase-memory-mcp");
    assert_eq!(relation["tool_version"], "synthetic-v1");
    assert_eq!(relation["to"], "checked.rs::local_only");
    assert_eq!(relation["checked_scope"]["kind"], "live_checkout");
    assert_eq!(relation["checked_scope"]["branch"], "main");
    assert_eq!(relation["checked_scope"]["origin"], "local");
}

#[test]
fn combined_branch_code_relations_do_not_certify_unchecked_occurrences() {
    let (r, report, revision) = code_scope_fixture();
    let live = cli(&r, &["brief"], Some(&report));
    let items = support::expanded_items(&live);
    assert_eq!(code_relation(&items[0])["status"], "current");
    for compact in [false, true] {
        let mut args = vec!["brief", "--branches", "main,feature"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, Some(&report)),
            mcp(
                &r,
                "brief",
                json!({"branches":["main","feature"],"compact":compact}),
                Some(&report),
            ),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 2);
            let feature = items
                .iter()
                .find(|item| item["observation"]["branch"] == "feature")
                .unwrap();
            assert_eq!(feature["text"], "Synthetic feature guide\n");
            assert_eq!(
                code_relation(feature)["status"],
                "outside_checked_scope",
                "a checkout report cannot certify the feature-only symbol state"
            );
            for item in items {
                assert_code_provenance(code_relation(&item), &revision);
            }
        }
    }
}

#[test]
fn incoming_code_relations_do_not_certify_unchecked_occurrences() {
    let (r, report, revision) = code_scope_fixture();
    let remote = r.bare_remote();
    support::git(
        &remote,
        &[
            "fetch",
            "-q",
            r.root.to_str().unwrap(),
            "feature:refs/heads/main",
        ],
    );
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    r.add_source(
        "[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=2\nmin_interval_seconds=300\n",
    );
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, Some(&report)),
            mcp(&r, "brief", json!({"compact":compact}), Some(&report)),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 2);
            for item in items {
                let relation = code_relation(&item);
                assert_code_provenance(relation, &revision);
                assert_eq!(
                    relation["status"],
                    if item["observation"]["origin"] == "incoming" {
                        "outside_checked_scope"
                    } else {
                        "current"
                    }
                );
            }
        }
    }
}

#[test]
fn a_named_checkout_snapshot_does_not_certify_working_only_code() {
    let (r, report_path, revision) = code_scope_fixture();
    r.write("checked.rs", "fn working_only() {}\n");
    let mut report: Value = serde_json::from_slice(&std::fs::read(&report_path).unwrap()).unwrap();
    report["files"][1]["content_sha256"] = json!(memq::util::hash(b"fn working_only() {}\n"));
    report["relations"][0]["to"] = json!("checked.rs::working_only");
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    assert_eq!(r.git(&["show", "HEAD:checked.rs"]), "fn local_only() {}");
    let live = cli(&r, &["brief"], Some(&report_path));
    assert_eq!(
        code_relation(&support::expanded_items(&live)[0])["status"],
        "current"
    );
    for compact in [false, true] {
        let mut args = vec!["brief", "--branches", "main"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, Some(&report_path)),
            mcp(
                &r,
                "brief",
                json!({"branches":["main"],"compact":compact}),
                Some(&report_path),
            ),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 1);
            assert_eq!(items[0]["observation"]["commit"], revision);
            let relation = code_relation(&items[0]);
            assert_eq!(relation["checked_revision"], revision);
            assert_eq!(relation["to"], "checked.rs::working_only");
            assert_eq!(relation["status"], "outside_checked_scope");
            assert_eq!(relation["report_status"], "current");
        }
    }
}

fn assert_unsupported_verifications_are_quoted(response: &Value, records: &Value) {
    let items = support::expanded_items(response);
    assert_eq!(items.len(), records.as_array().unwrap().len());
    assert!(
        response["coverage"]["sources_paused"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for item in items {
        let quoted: Value = serde_json::from_str(item["text"].as_str().unwrap()).unwrap();
        let expected = records
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == quoted["id"])
            .unwrap();
        assert_eq!(quoted["verification"], expected["verification"]);
        assert_eq!(item["untrusted"], true);
        assert!(item.get("verification").is_none());
        assert!(item.get("claim").is_none());
        assert!(item.get("applicability").is_none());
        assert!(
            item["flags"]
                .as_array()
                .unwrap()
                .contains(&json!("unsupported_verification_schema"))
        );
    }
}

fn unsupported_verification_shapes() -> Value {
    json!([
        {"id":"string","text":"Synthetic report field","verification":"pending"},
        {"id":"number","text":"Synthetic report field","verification":42},
        {"id":"list","text":"Synthetic report field","verification":["pending"]},
        {"id":"boolean","text":"Synthetic report field","verification":true}
    ])
}

#[test]
fn cli_quotes_non_object_verification_fields_without_claims_or_panics() {
    let r = Repo::initialized();
    r.record_source();
    let records = unsupported_verification_shapes();
    r.records(records.clone());
    for args in [vec!["brief"], vec!["brief", "--compact"]] {
        let response = cli(&r, &args, None);
        assert_unsupported_verifications_are_quoted(&response, &records);
    }
}

#[test]
fn mcp_quotes_non_object_verification_fields_without_claims_or_panics() {
    let r = Repo::initialized();
    r.record_source();
    let records = unsupported_verification_shapes();
    r.records(records.clone());
    for compact in [false, true] {
        let response = mcp(&r, "brief", json!({"compact":compact}), None);
        assert_unsupported_verifications_are_quoted(&response, &records);
    }
}

#[test]
fn matching_hashes_do_not_turn_an_incomplete_verification_into_a_report() {
    let r = Repo::initialized();
    r.record_source();
    r.write("checked.rs", "Synthetic checked content\n");
    let records = json!([{
        "id":"incomplete","text":"Synthetic incomplete report",
        "verification":{
            "evidence":["checked.rs"],
            "evidence_content":[{
                "path":"checked.rs",
                "content_sha256":memq::util::hash(b"Synthetic checked content\n")
            }]
        }
    }]);
    r.records(records.clone());
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            assert_unsupported_verifications_are_quoted(&response, &records);
        }
    }
}

fn verification_report(r: &Repo) -> Value {
    json!({
        "command":"synthetic check","revision":r.git(&["rev-parse","HEAD"]),
        "object_format":"sha1","environment":"synthetic environment",
        "result":"reported pass","reported_by":"fixture",
        "evidence":["checked.rs"],
        "evidence_content":[{
            "path":"checked.rs",
            "content_sha256":memq::util::hash(b"Synthetic checked content\n")
        }]
    })
}

#[test]
fn verification_requires_every_report_field_and_valid_evidence() {
    let r = Repo::initialized();
    r.record_source();
    r.write("checked.rs", "Synthetic checked content\n");
    let report = verification_report(&r);
    let mut records = Vec::new();
    for field in [
        "command",
        "revision",
        "object_format",
        "environment",
        "result",
        "reported_by",
        "evidence",
        "evidence_content",
    ] {
        let mut missing = report.clone();
        missing.as_object_mut().unwrap().remove(field);
        records.push(json!({
            "id":format!("missing-{field}"),"text":"Synthetic missing report field",
            "verification":missing
        }));
        let mut invalid = report.clone();
        invalid[field] = json!(false);
        records.push(json!({
            "id":format!("invalid-{field}"),"text":"Synthetic invalid report field",
            "verification":invalid
        }));
    }
    for (id, pointer, value) in [
        ("blank-command", "/command", json!(" ")),
        ("blank-reporter", "/reported_by", json!(" ")),
        ("revision-format", "/revision", json!("unknown")),
        ("object-format", "/object_format", json!("unsupported")),
        ("evidence-value", "/evidence/0", json!(42)),
        (
            "evidence-hash",
            "/evidence_content/0/content_sha256",
            json!("bad"),
        ),
        (
            "evidence-path",
            "/evidence_content/0/path",
            json!("../outside.rs"),
        ),
    ] {
        let mut invalid = report.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        records.push(json!({
            "id":id,"text":"Synthetic invalid report evidence","verification":invalid
        }));
    }
    let mut invalid_scope = report;
    invalid_scope["evidence_scope_complete"] = json!("false");
    records.push(json!({
        "id":"invalid-scope","text":"Synthetic invalid report scope",
        "verification":invalid_scope
    }));
    let records = json!(records);
    r.records(records.clone());
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            assert_unsupported_verifications_are_quoted(&response, &records);
        }
    }
}

#[test]
fn valid_imported_verification_rechecks_current_stale_and_unknown_on_retained_reads() {
    let r = Repo::initialized();
    r.record_source();
    r.write("checked.rs", "Synthetic checked content\n");
    let report = verification_report(&r);
    r.records(json!([{
        "id":"supported","text":"Synthetic supported verification","verification":report
    }]));
    let source_bytes = std::fs::read(r.root.join("records.json")).unwrap();
    let original = cli(&r, &["brief"], None);
    let view = original["freshness"]["view_id"].as_str().unwrap();
    let original_item = &support::expanded_items(&original)[0];
    let id = original_item["id"].as_str().unwrap();
    for expected in ["current", "stale", "unknown"] {
        match expected {
            "stale" => r.write("checked.rs", "Synthetic changed content\n"),
            "unknown" => std::fs::remove_file(r.root.join("checked.rs")).unwrap(),
            _ => (),
        }
        for compact in [false, true] {
            for (mut args, tool, arguments) in [
                (
                    vec!["brief", "--view-id", view],
                    "brief",
                    json!({"view_id":view,"compact":compact}),
                ),
                (
                    vec![
                        "search",
                        "Synthetic",
                        "--incoming",
                        "true",
                        "--view-id",
                        view,
                    ],
                    "search",
                    json!({"query":"Synthetic","incoming":true,"view_id":view,"compact":compact}),
                ),
                (
                    vec!["show", id, "--view-id", view],
                    "show",
                    json!({"ids":[id],"view_id":view,"compact":compact}),
                ),
            ] {
                if compact {
                    args.push("--compact");
                }
                for response in [cli(&r, &args, None), mcp(&r, tool, arguments, None)] {
                    let items = support::expanded_items(&response);
                    assert_eq!(items.len(), 1);
                    let item = &items[0];
                    assert_eq!(item["claim"], "reported");
                    assert_eq!(item["applicability"], expected);
                    assert_eq!(item["verification"]["applicability"], expected);
                    assert_eq!(
                        item["verification"]["environment_applicability"],
                        "not_checked"
                    );
                    for (field, value) in report.as_object().unwrap() {
                        assert_eq!(&item["verification"][field], value);
                    }
                    for field in ["id", "pointer", "text", "acceptance", "attribution"] {
                        assert_eq!(item[field], original_item[field]);
                    }
                    assert_eq!(response["freshness"]["view_id"], view);
                }
            }
        }
    }
    assert_eq!(
        std::fs::read(r.root.join("records.json")).unwrap(),
        source_bytes
    );
}

#[test]
fn valid_reports_with_incomplete_evidence_scope_remain_reported_but_unknown() {
    let r = Repo::initialized();
    r.record_source();
    r.write("checked.rs", "Synthetic checked content\n");
    let report = verification_report(&r);
    let mut uncovered = report.clone();
    uncovered["evidence"] = json!(["checked.rs", "uncovered.rs"]);
    let mut empty = report.clone();
    empty["evidence"] = json!([]);
    empty["evidence_content"] = json!([]);
    let mut incomplete = report;
    incomplete["evidence_scope_complete"] = json!(false);
    r.records(json!([
        {"id":"uncovered","verification":uncovered},
        {"id":"empty","verification":empty},
        {"id":"incomplete-scope","verification":incomplete}
    ]));
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 3);
            for item in items {
                assert_eq!(item["claim"], "reported");
                assert_eq!(item["applicability"], "unknown");
                assert_eq!(item["verification"]["reported_by"], "fixture");
            }
        }
    }
}

fn incoming_records(local: Value, incoming: Value) -> Repo {
    let r = Repo::initialized();
    r.record_source();
    r.records(local);
    r.commit("synthetic local decisions");
    r.git(&["switch", "-q", "-c", "upstream"]);
    r.records(incoming);
    r.commit("synthetic incoming decisions");
    r.git(&["switch", "-q", "main"]);
    let remote = r.bare_remote();
    support::git(
        &remote,
        &[
            "fetch",
            "-q",
            r.root.to_str().unwrap(),
            "upstream:refs/heads/main",
        ],
    );
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    r.add_source(
        "[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=2\nmin_interval_seconds=300\n",
    );
    r
}

#[test]
fn accepted_or_inactive_same_id_upstream_edits_preserve_local_decisions() {
    let local = json!([
        {"id":"accepted","status":"accepted","decider":"owner","reason":"Synthetic local choice"},
        {"id":"withdrawn","status":"withdrawn","decider":"owner","reason":"Synthetic withdrawal"},
        {"id":"unknown","status":"unlisted","decider":"owner","reason":"Synthetic unlisted state"}
    ]);
    let incoming = json!([
        {"id":"accepted","status":"accepted","decider":"owner","reason":"Synthetic incoming wording"},
        {"id":"withdrawn","status":"accepted","decider":"owner","reason":"Synthetic incoming choice"},
        {"id":"unknown","status":"accepted","decider":"owner","reason":"Synthetic incoming choice"}
    ]);
    let r = incoming_records(local.clone(), incoming.clone());
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 6);
            for item in items {
                let record: Value = serde_json::from_str(item["text"].as_str().unwrap()).unwrap();
                let local_occurrence = item["observation"]["origin"] == "local";
                let source = if local_occurrence { &local } else { &incoming };
                assert_eq!(
                    &record,
                    source
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|expected| expected["id"] == record["id"])
                        .unwrap()
                );
                assert_eq!(
                    item["section"],
                    if !local_occurrence || record["id"] == "accepted" {
                        "accepted_decisions"
                    } else {
                        "evidence"
                    }
                );
                assert!(item["supersession"].is_null());
                assert!(item["conflict_with"].as_array().is_none_or(Vec::is_empty));
                assert!(!item["flags"].as_array().is_some_and(|flags| {
                    flags.contains(&json!("conflict"))
                        || flags.contains(&json!("supersession_conflict"))
                }));
            }
        }
    }
}

#[test]
fn pending_same_id_upstream_resolutions_keep_both_versions_without_self_supersession() {
    let r = incoming_records(
        json!([
            {"id":"pending","status":"proposed","decider":"owner","reason":"Synthetic pending choice"},
            {"id":"unverified","status":"accepted","reason":"Synthetic unattributed choice"}
        ]),
        json!([
            {"id":"pending","status":"accepted","decider":"maintainer","reason":"Synthetic approval"},
            {"id":"unverified","status":"accepted","decider":"maintainer","reason":"Synthetic attribution"}
        ]),
    );
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 4);
            for item in items {
                if item["observation"]["origin"] == "local" {
                    assert_eq!(item["section"], "resolved_upstream_not_local");
                    assert_eq!(
                        item["acceptance"],
                        if item["id"].as_str().unwrap().ends_with(":pending") {
                            "proposed"
                        } else {
                            "unverified"
                        }
                    );
                } else {
                    assert_eq!(item["acceptance"], "accepted");
                }
                assert!(
                    item["supersession"].is_null(),
                    "a new version of the same identity is not a self-replacement"
                );
                assert!(item["conflict_with"].as_array().is_none_or(Vec::is_empty));
            }
        }
    }
}

#[test]
fn thousands_of_non_decisions_preserve_conflicts_proposals_and_authored_links() {
    let r = Repo::initialized();
    r.record_source();
    r.add_source(
        "[[source]]\nid='history'\nkind='json-records'\npath='history.json'\ncollection='records'\nid_field='id'\n",
    );
    r.write(
        "history.json",
        serde_json::to_vec(&json!({
            "records": (0..3000).map(|index| json!({
                "id":format!("event-{index:04}"),"text":format!("Synthetic history entry {index:04}")
            })).collect::<Vec<_>>()
        }))
        .unwrap(),
    );
    r.records(json!([
        {"id":"old","status":"accepted","decider":"owner"},
        {"id":"choice-a","status":"accepted","decider":"owner","supersedes":"old"},
        {"id":"choice-b","status":"accepted","decider":"owner","supersedes":"old"},
        {"id":"proposal","status":"proposed","decider":"owner","supersedes":"old"},
        {"id":"linked","status":"proposed","decider":"owner","depends_on":"old"},
        {"id":"independent","status":"accepted","decider":"owner"}
    ]));
    r.commit("synthetic history and decision graph");
    let response = cli(&r, &["brief", "--compact"], None);
    let project = response["scope"]["project_id"].as_str().unwrap();
    let view = response["freshness"]["view_id"].as_str().unwrap();
    let id = |native: &str| format!("mq:{project}:records:{native}");
    let selected = [
        id("old"),
        id("choice-a"),
        id("choice-b"),
        id("proposal"),
        id("linked"),
        id("independent"),
        format!("mq:{project}:history:event-0000"),
        format!("mq:{project}:history:event-2999"),
    ];
    for compact in [false, true] {
        let mut args = vec!["show"];
        args.extend(selected.iter().map(String::as_str));
        args.extend(["--view-id", view]);
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(
                &r,
                "show",
                json!({"ids":selected,"view_id":view,"compact":compact}),
                None,
            ),
        ] {
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), selected.len());
            let find = |id: &str| items.iter().find(|item| item["id"] == id).unwrap();
            let old = find(&id("old"));
            assert_eq!(old["acceptance"], "accepted");
            assert!(old["supersession"].is_null());
            assert!(
                old["flags"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("supersession_conflict"))
            );
            assert!(
                old["flags"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(format!("proposed_replacement: {}", id("proposal"))))
            );
            for (left, right) in [("choice-a", "choice-b"), ("choice-b", "choice-a")] {
                let choice = find(&id(left));
                assert_eq!(choice["conflict_with"], json!([id(right)]));
                assert!(
                    choice["flags"]
                        .as_array()
                        .unwrap()
                        .contains(&json!("conflict"))
                );
            }
            let linked = find(&id("linked"));
            assert_eq!(linked["relations"][0]["to"], id("old"));
            assert_eq!(linked["relations"][0]["status"], "resolved");
            for key in [id("proposal"), id("linked"), id("independent")] {
                assert!(
                    find(&key)["conflict_with"]
                        .as_array()
                        .is_none_or(Vec::is_empty)
                );
            }
            for history in &selected[6..] {
                let item = find(history);
                assert_eq!(item["acceptance"], "n/a");
                assert_eq!(item["section"], "evidence");
                assert!(item["supersession"].is_null());
                assert!(item["conflict_with"].as_array().is_none_or(Vec::is_empty));
            }
        }
    }
}

#[test]
fn scoped_reference_resolution_preserves_inverse_links_annotations_and_ambiguity() {
    let r = Repo::initialized();
    r.record_source();
    r.add_source("superseded_by_field='superseded_by'\n");
    r.records(json!([
        {"id":"base","status":"accepted","decider":"owner"},
        {"id":"annotated","status":"accepted","decider":"owner","supersedes":"base (synthetic rationale)"},
        {"id":"reverse-old","status":"accepted","decider":"owner","superseded_by":"reverse-new"},
        {"id":"reverse-new","status":"accepted","decider":"owner"},
        {"id":"proposed-target","status":"accepted","decider":"owner","superseded_by":"proposal"},
        {"id":"proposal","status":"proposed","decider":"owner"},
        {"id":"ambiguous-a","status":"accepted","decider":"owner"},
        {"id":"ambiguous-b","status":"accepted","decider":"owner"},
        {"id":"unresolved","status":"accepted","decider":"owner","supersedes":"ambiguous-a, ambiguous-b"}
    ]));
    for compact in [false, true] {
        let mut args = vec!["brief"];
        if compact {
            args.push("--compact");
        }
        for response in [
            cli(&r, &args, None),
            mcp(&r, "brief", json!({"compact":compact}), None),
        ] {
            let project = response["scope"]["project_id"].as_str().unwrap();
            let id = |native: &str| format!("mq:{project}:records:{native}");
            let items = support::expanded_items(&response);
            assert_eq!(items.len(), 9);
            let find = |native: &str| items.iter().find(|item| item["id"] == id(native)).unwrap();
            for (old, replacement) in [("base", "annotated"), ("reverse-old", "reverse-new")] {
                assert_eq!(find(old)["supersession"], id(replacement));
                assert_eq!(find(old)["section"], "superseded");
            }
            assert_eq!(find("reverse-old")["relations"][0]["to"], id("reverse-new"));
            assert_eq!(find("reverse-old")["relations"][0]["status"], "resolved");
            assert!(find("proposed-target")["supersession"].is_null());
            assert_eq!(find("proposed-target")["acceptance"], "accepted");
            assert!(
                find("proposed-target")["flags"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(format!("proposed_replacement: {}", id("proposal"))))
            );
            assert_eq!(
                find("unresolved")["relations"][0]["status"],
                "ambiguous_endpoint"
            );
            for native in ["ambiguous-a", "ambiguous-b", "unresolved"] {
                assert!(find(native)["supersession"].is_null());
                assert_eq!(find(native)["acceptance"], "accepted");
            }
            assert!(
                items
                    .iter()
                    .all(|item| item["conflict_with"].as_array().is_none_or(Vec::is_empty))
            );
        }
    }
}
