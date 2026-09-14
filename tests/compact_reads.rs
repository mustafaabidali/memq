mod support;

use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;
use support::{Repo, expanded_items, git};

fn checked(raw: &[u8]) -> Value {
    let value: Value = serde_json::from_slice(raw).unwrap();
    let text = std::str::from_utf8(raw).unwrap();
    let used = match value["budget"]["kind"].as_str().unwrap() {
        "bytes" => raw.len(),
        _ if value["budget"]["encoding"] == "cl100k_base" => tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(text)
            .len(),
        _ => tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len(),
    };
    assert_eq!(value["budget"]["used"], used);
    assert!(used <= value["budget"]["limit"].as_u64().unwrap() as usize);
    value
}

fn cli(r: &Repo, args: &[&str]) -> Value {
    let out = r.run(args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    checked(&out.stdout)
}

fn same_evidence(full: &Value, compact: &Value) {
    for key in [
        "id",
        "pointer",
        "text",
        "availability",
        "section",
        "native_status",
        "acceptance",
        "attribution",
        "committed",
        "verification",
        "claim",
        "applicability",
        "evidence_view_id",
        "absent_since",
        "text_range",
        "total_bytes",
        "complete",
    ] {
        assert_eq!(compact[key], full[key], "changed {key}");
    }
    for key in ["flags", "conflict_with", "supersession", "reason"] {
        let value = &full[key];
        if !value.is_null() && !value.as_array().is_some_and(Vec::is_empty) {
            assert_eq!(compact[key], *value, "lost nonempty {key}");
        }
    }
    if let Some(observation) = full.get("observation") {
        for key in [
            "origin",
            "branch",
            "commit",
            "object_format",
            "dirty",
            "native_revision",
        ] {
            assert_eq!(
                compact["observation"][key], observation[key],
                "changed observation {key}"
            );
        }
    }
    let full_relations = full["relations"].as_array().cloned().unwrap_or_default();
    let compact_relations = compact["relations"].as_array().cloned().unwrap_or_default();
    assert_eq!(full_relations.len(), compact_relations.len());
    for (a, b) in full_relations.iter().zip(&compact_relations) {
        if a["kind"] == "authored" {
            for key in ["kind", "field", "to", "status"] {
                assert_eq!(a[key], b[key], "changed authored {key}");
            }
            assert!(b.get("from_version_id").is_none());
            assert!(b.get("to_version_id").is_none());
        } else {
            assert_eq!(a, b, "non-authored provenance must stay complete");
        }
    }
}

#[test]
fn compact_cli_preserves_evidence_and_full_details_in_the_same_view() {
    let r = Repo::initialized();
    r.record_source();
    let secret = format!("sk-proj-{}", "aB7xZ9".repeat(9));
    r.records(json!([
        {"id":"old","status":"accepted","decider":"owner","reason":"Keep the gate"},
        {"id":"proposal","status":"proposed","decider":"owner","supersedes":"old","reason":"Proposed change"},
        {"id":"unknown","status":"accepted","decider":"contributor","depends_on":"absent","api_key":secret},
        {"id":"replacement","status":"accepted","decider":"maintainer","supersedes":"old","reason":"First replacement"},
        {"id":"rival","status":"accepted","decider":"owner","supersedes":"old","reason":"Conflicting replacement"}
    ]));
    let full = cli(&r, &["brief", "--budget", "30000"]);
    let view = full["freshness"]["view_id"].as_str().unwrap();
    let compact = cli(
        &r,
        &["brief", "--compact", "--view-id", view, "--budget", "30000"],
    );
    assert!(full["memq"].get("representation").is_none());
    assert_eq!(compact["memq"]["representation"], "compact");
    assert!(compact["budget"]["used"].as_u64() < full["budget"]["used"].as_u64());
    for key in [
        "scope",
        "freshness",
        "coverage",
        "changes_since",
        "incomplete",
        "reason",
        "omitted_count",
        "omitted",
        "continuation",
        "untrusted_notice",
    ] {
        assert_eq!(compact[key], full[key], "changed top-level {key}");
    }
    let items = expanded_items(&compact);
    assert_eq!(items.len(), 5);
    for (a, b) in full["items"].as_array().unwrap().iter().zip(&items) {
        same_evidence(a, b);
        assert!(b.get("version_id").is_none());
        assert!(b["observation"].get("observation_id").is_none());
    }
    let wire = serde_json::to_string(&compact).unwrap();
    assert!(!wire.contains(&secret));
    assert!(wire.contains("[REDACTED]"));
    assert!(items.iter().any(|item| item["acceptance"] == "unverified"));
    assert!(items.iter().any(|item| {
        item["flags"]
            .as_array()
            .is_some_and(|flags| flags.contains(&json!("conflict")))
    }));
    let shown = cli(
        &r,
        &[
            "show",
            items[0]["id"].as_str().unwrap(),
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    assert!(shown["items"][0].get("version_id").is_some());
    assert!(
        shown["items"][0]["observation"]
            .get("observation_id")
            .is_some()
    );
    assert_eq!(shown["items"][0]["text"], items[0]["text"]);
    assert_eq!(shown["items"][0]["pointer"], items[0]["pointer"]);
}

#[test]
fn compact_budget_selection_evidence_pages_and_continuations_are_representation_bound() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!((0..25).map(|n| json!({
        "id":format!("required-{n}"), "status":"blocked", "reason":"Keep this required evidence",
        "depends_on":format!("required-{}",(n+1)%25), "text":"مرحبا evidence"
    })).collect::<Vec<_>>()));
    let all = cli(&r, &["brief", "--budget", "100000"]);
    let view = all["freshness"]["view_id"].as_str().unwrap();
    for (kind, encoding, limit) in [
        ("tokens", "o200k_base", "2200"),
        ("tokens", "cl100k_base", "2200"),
        ("bytes", "o200k_base", "6000"),
    ] {
        let base = [
            "brief",
            "--view-id",
            view,
            "--budget-kind",
            kind,
            "--tokenizer",
            encoding,
            "--budget",
            limit,
        ];
        let full = cli(&r, &base);
        let mut args = base.to_vec();
        args.push("--compact");
        let first = cli(&r, &args);
        assert!(
            expanded_items(&first).len() > full["items"].as_array().unwrap().len(),
            "projection must happen before budget selection"
        );
        assert_eq!(first["incomplete"], true);
        assert_eq!(first["reason"], "required_items_omitted");
        r.error(
            &[
                "brief",
                "--continuation",
                first["continuation"].as_str().unwrap(),
            ],
            "stale_continuation",
        );
        r.error(
            &[
                "brief",
                "--compact",
                "--continuation",
                full["continuation"].as_str().unwrap(),
            ],
            "stale_continuation",
        );
        let mut recovered = std::collections::BTreeMap::<String, String>::new();
        let mut order = Vec::new();
        let mut page = first;
        let mut pages = 0;
        loop {
            let evidence = expanded_items(&page);
            assert!(!evidence.is_empty(), "every page must carry evidence");
            assert!(page["omitted"].as_array().unwrap().is_empty());
            for item in evidence {
                let id = item["id"].as_str().unwrap().to_owned();
                if !recovered.contains_key(&id) {
                    order.push(id.clone());
                }
                let text = recovered.entry(id).or_default();
                if item.get("text_range").is_some() {
                    assert_eq!(item["text_range"][0], text.len());
                } else {
                    assert!(text.is_empty(), "whole evidence must not repeat");
                }
                text.push_str(item["text"].as_str().unwrap());
                if item.get("text_range").is_some() {
                    assert_eq!(item["text_range"][1], text.len());
                }
            }
            let Some(next) = page["continuation"].as_str() else {
                break;
            };
            assert_eq!(
                memq::budget::Continuation::decode(next).unwrap().stream,
                "brief-evidence"
            );
            let mut next_args = args.clone();
            next_args.extend(["--continuation", next]);
            page = cli(&r, &next_args);
            pages += 1;
            assert!(pages < 100, "evidence pages must progress");
        }
        let expected = all["items"].as_array().unwrap();
        assert_eq!(
            order,
            expected
                .iter()
                .map(|i| i["id"].as_str().unwrap())
                .collect::<Vec<_>>()
        );
        assert_eq!(recovered.len(), expected.len());
        for item in expected {
            assert_eq!(recovered[item["id"].as_str().unwrap()], item["text"]);
        }
    }
}

#[test]
fn compact_mcp_and_cli_match_for_all_reads_in_both_transports() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"Original quoted evidence","status":"blocked"}]));
    let saved = cli(&r, &["brief"]);
    let view = saved["freshness"]["view_id"].as_str().unwrap();
    let id = saved["items"][0]["id"].as_str().unwrap();
    // Keep a deliberately retained view, so startup reconciliation and accesses
    // cannot change the current-versus-retained status between interface calls.
    r.records(json!([{"id":"one","text":"New current evidence","status":"blocked"}]));
    r.ok(&["brief"]);
    for fallback in [false, true] {
        for (operation, extra, extra_cli) in [
            ("brief", json!({}), Vec::new()),
            ("search", json!({"query":"evidence"}), vec!["evidence"]),
            ("show", json!({"ids":[id]}), vec![id]),
        ] {
            for kind in ["tokens", "bytes"] {
                let mut args =
                    json!({"compact":true,"view_id":view,"budget_kind":kind,"budget":16000});
                args.as_object_mut()
                    .unwrap()
                    .extend(extra.as_object().unwrap().clone());
                let mut command = vec![operation];
                command.extend(extra_cli.clone());
                command.extend([
                    "--compact",
                    "--view-id",
                    view,
                    "--budget-kind",
                    kind,
                    "--budget",
                    "16000",
                ]);
                let expected = cli(&r, &command);
                let mut child_command = r.command();
                child_command.arg("mcp");
                if fallback {
                    child_command.arg("--text-fallback");
                }
                let mut child = child_command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut input = child.stdin.take().unwrap();
                for request in [
                    json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
                    json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":operation,"arguments":args}}),
                ] {
                    writeln!(input, "{request}").unwrap();
                }
                drop(input);
                let out = child.wait_with_output().unwrap();
                assert!(out.status.success());
                let messages: Vec<Value> = std::str::from_utf8(&out.stdout)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                for tool in messages[0]["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|tool| tool["name"] != "note")
                {
                    assert_eq!(
                        tool["inputSchema"]["properties"]["compact"]["type"],
                        "boolean"
                    );
                }
                let result = &messages[1]["result"];
                assert_eq!(result["isError"], false);
                let actual = if fallback {
                    assert!(result.get("structuredContent").is_none());
                    assert_eq!(result["content"].as_array().unwrap().len(), 1);
                    checked(result["content"][0]["text"].as_str().unwrap().as_bytes())
                } else {
                    assert!(result["content"].as_array().unwrap().is_empty());
                    checked(&serde_json::to_vec(&result["structuredContent"]).unwrap())
                };
                assert_eq!(actual, expected);
                assert_eq!(actual["freshness"]["status"], "stale");
            }
        }
    }
}

#[test]
fn compact_keeps_each_incoming_branch_member_and_external_relation_provenance() {
    let r = Repo::initialized();
    r.record_source();
    r.write("callback.rs", "fn callback() { exchange(); }\n");
    r.records(json!([{"id":"decision","status":"accepted","decider":"owner","reason":"First mode","file":"callback.rs"}]));
    r.commit("synthetic compact input");
    let side = r.linked("side");
    std::fs::write(side.join("records.json"), serde_json::to_vec(&json!({"records":[
        {"id":"decision","status":"accepted","decider":"maintainer","reason":"Other mode","file":"callback.rs"}
    ]})).unwrap()).unwrap();
    git(&side, &["add", "records.json"]);
    git(
        &side,
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "synthetic other mode",
        ],
    );
    let remote = r.bare_remote();
    git(
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
    let ready = r.ok(&["brief", "--branches", "main,side", "--budget", "30000"]);
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let report = json!({
        "format":1,"tool":"codebase-memory-mcp","tool_version":"synthetic-v1",
        "report_id":"synthetic-compact-coverage","project_id":ready["scope"]["project_id"],
        "worktree":memq::util::hash(repo.root.to_string_lossy().as_bytes()),
        "checked_revision":r.git(&["rev-parse","HEAD"]),"object_format":"sha1","complete":false,
        "files":[{"path":"callback.rs","content_sha256":memq::util::hash(b"fn callback() { exchange(); }\n")}],
        "relations":[{"from":"callback.rs","to":"exchange","relationship":"calls"}]
    });
    let report_path = r.temp.path().join("code-report.json");
    std::fs::write(&report_path, serde_json::to_vec(&report).unwrap()).unwrap();
    let out = r
        .command()
        .env("MEMQ_CODE_REPORT", &report_path)
        .args(["brief", "--branches", "main,side", "--budget", "30000"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let with_code = checked(&out.stdout);
    let view = with_code["freshness"]["view_id"].as_str().unwrap();

    // Extend this synthetic retained-view fixture with future relation kinds.
    // This checks presentation loss only; it is not semantic validation.
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let (seq, text): (String, String) = db.query_row(
        "SELECT member_seq,member_json FROM view_items WHERE view_id=?1 ORDER BY member_seq LIMIT 1",
        [view], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    let mut member: Value = serde_json::from_str(&text).unwrap();
    let extra = [
        json!({"kind":"semantic","model":"synthetic-model","model_revision":"synthetic-build","status":"related","score":0.7,"to_version_id":"synthetic-version","provenance":{"input":"synthetic-input"}}),
        json!({"kind":"future-relation","field":"unknown-field","from_version_id":"keep-this","to_version_id":"keep-that","status":"unknown","warning":"unverified extension"}),
    ];
    member["relations"]
        .as_array_mut()
        .unwrap()
        .extend(extra.clone());
    db.execute(
        "UPDATE view_items SET member_json=?1 WHERE view_id=?2 AND member_seq=?3",
        rusqlite::params![serde_json::to_string(&member).unwrap(), view, seq],
    )
    .unwrap();
    drop(db);
    let full = cli(
        &r,
        &[
            "brief",
            "--branches",
            "main,side",
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    let compact = cli(
        &r,
        &[
            "brief",
            "--compact",
            "--branches",
            "main,side",
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    assert_eq!(full["coverage"], compact["coverage"]);
    assert_eq!(compact["coverage"]["code"]["status"], "partial");
    let items = expanded_items(&compact);
    assert_eq!(items.len(), 3);
    assert!(
        items
            .iter()
            .any(|item| item["observation"]["origin"] == "incoming")
    );
    assert!(
        items
            .iter()
            .any(|item| item["observation"]["branch"] == "side")
    );
    assert!(items.iter().any(|item| {
        item["flags"]
            .as_array()
            .is_some_and(|flags| flags.contains(&json!("conflict")))
    }));
    for (a, b) in full["items"].as_array().unwrap().iter().zip(&items) {
        same_evidence(a, b);
    }
    let relations: Vec<_> = items
        .iter()
        .flat_map(|item| item["relations"].as_array().unwrap())
        .collect();
    assert!(relations.iter().any(|relation| relation["kind"] == "code"));
    for relation in &extra {
        assert!(relations.contains(&relation));
    }
}

#[test]
fn compact_verification_keeps_reported_claims_and_current_stale_unknown_applicability() {
    let r = Repo::initialized();
    r.write("checked.rs", "synthetic code");
    let report = json!({
        "command":"synthetic check","revision":r.git(&["rev-parse","HEAD"]),"object_format":"sha1",
        "environment":"synthetic environment","result":"reported pass","reported_by":"fixture",
        "evidence":["checked.rs"],"evidence_content":[{"path":"checked.rs","content_sha256":memq::util::hash(b"synthetic code")}]
    });
    r.write("verification.json", serde_json::to_vec(&report).unwrap());
    let first = r.ok(&[
        "note",
        "--kind",
        "verification",
        "--text",
        "Reported check",
        "--idempotency-key",
        "first",
        "--verification",
        r.root.join("verification.json").to_str().unwrap(),
    ]);
    let mut partial = report;
    partial["evidence"] = json!(["checked.rs", "unrecorded.rs"]);
    r.write("verification.json", serde_json::to_vec(&partial).unwrap());
    let unknown = r.ok(&[
        "note",
        "--kind",
        "verification",
        "--text",
        "Partial report",
        "--idempotency-key",
        "unknown",
        "--verification",
        r.root.join("verification.json").to_str().unwrap(),
    ]);
    let ids = [
        first["id"].as_str().unwrap(),
        unknown["id"].as_str().unwrap(),
    ];
    for expected in ["current", "stale"] {
        if expected == "stale" {
            r.write("checked.rs", "changed code");
        }
        let current = r.ok(&["brief", "--budget", "30000"]);
        let view = current["freshness"]["view_id"].as_str().unwrap();
        let full = cli(
            &r,
            &[
                "show",
                ids[0],
                ids[1],
                "--view-id",
                view,
                "--budget",
                "30000",
            ],
        );
        let compact = cli(
            &r,
            &[
                "show",
                ids[0],
                ids[1],
                "--compact",
                "--view-id",
                view,
                "--budget",
                "30000",
            ],
        );
        let compact_items = expanded_items(&compact);
        assert_eq!(compact_items[0]["applicability"], expected);
        assert_eq!(compact_items[1]["applicability"], "unknown");
        for (a, b) in full["items"].as_array().unwrap().iter().zip(&compact_items) {
            same_evidence(a, b);
            assert_eq!(b["claim"], "reported");
            assert_eq!(
                b["verification"]["environment_applicability"],
                "not_checked"
            );
        }
    }
}

#[test]
fn compact_capture_pagination_keeps_utf8_native_claims_missing_sources_and_tombstones() {
    let r = Repo::initialized();
    let store = r.temp.path().join("sessions");
    std::fs::create_dir(&store).unwrap();
    let source = store.join("synthetic.jsonl");
    let header = json!({"type":"session_meta","ordinal":0,"payload":{
        "id":"synthetic-compact-session","cwd":r.root,"cli_version":"0.154.0",
        "git":{"branch":"main","commit_hash":"reported-native-revision"}
    }});
    let record = |ordinal, text: String| {
        json!({
            "type":"response_item","ordinal":ordinal,"timestamp":"2026-09-13T19:01:00Z",
            "payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}
        })
    };
    std::fs::write(
        &source,
        format!(
            "{header}\n{}\n{}\n",
            record(1, "مرحبا 🐬 quoted \"text\"\n".repeat(400)),
            record(2, "Short final evidence".into())
        ),
    )
    .unwrap();
    r.add_source(&format!("[capture]\ncodex={}", json!(store)));
    let initial = r.ok(&["brief", "--budget", "30000"]);
    let view = initial["freshness"]["view_id"].as_str().unwrap();
    let ids: Vec<_> = initial["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    let full = cli(
        &r,
        &[
            "show",
            ids[0],
            ids[1],
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    let originals = full["items"].as_array().unwrap();
    let mut next: Option<String> = None;
    let mut restored = [String::new(), String::new()];
    let mut index = 0;
    let mut pages = 0;
    loop {
        let mut args = vec![
            "show",
            ids[0],
            ids[1],
            "--compact",
            "--view-id",
            view,
            "--budget-kind",
            "bytes",
            "--budget",
            "3500",
        ];
        if let Some(cursor) = next.as_deref() {
            args.extend(["--continuation", cursor]);
        }
        let page = cli(&r, &args);
        for item in expanded_items(&page) {
            assert_eq!(item["id"], ids[index]);
            assert_eq!(item["pointer"], originals[index]["pointer"]);
            assert_eq!(
                item["observation"]["native_revision"],
                header["payload"]["git"]
            );
            assert!(
                item.get("claim").is_none(),
                "native revision must not create a verification claim"
            );
            assert_eq!(item["text_range"][0], restored[index].len());
            restored[index].push_str(item["text"].as_str().unwrap());
            assert_eq!(item["text_range"][1], restored[index].len());
            if item["complete"] == true {
                assert_eq!(restored[index], originals[index]["text"].as_str().unwrap());
                index += 1;
            }
        }
        pages += 1;
        assert!(pages < 100);
        let Some(cursor) = page["continuation"].as_str() else {
            break;
        };
        if pages == 1 {
            r.error(
                &["show", ids[0], ids[1], "--continuation", cursor],
                "stale_continuation",
            );
        }
        next = Some(cursor.to_owned());
    }
    assert!(pages > 1, "fixture must exercise compact partial pages");
    assert_eq!(index, 2);
    std::fs::remove_file(&source).unwrap();
    let missing = cli(
        &r,
        &[
            "show",
            ids[0],
            ids[1],
            "--compact",
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    for (original, item) in originals.iter().zip(expanded_items(&missing)) {
        assert_eq!(item["availability"], "source_missing");
        assert_eq!(item["pointer"], original["pointer"]);
        assert_eq!(item["text"], original["text"]);
    }
    r.ok(&["forget", ids[0]]);
    let forgotten = cli(
        &r,
        &[
            "show",
            ids[0],
            ids[1],
            "--compact",
            "--view-id",
            view,
            "--budget",
            "30000",
        ],
    );
    assert_eq!(
        expanded_items(&forgotten)[0],
        json!({"id":ids[0],"availability":"forgotten"})
    );
    assert_eq!(expanded_items(&forgotten)[1]["text"], originals[1]["text"]);
}
