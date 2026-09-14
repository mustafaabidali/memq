mod support;

use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;
use support::{Repo, expanded_items, git};

fn shared_project() -> Repo {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!(
        (0..12)
            .map(|n| json!({
                "id":format!("decision-{n}"),
                "status":"accepted",
                "decider":"owner",
                "text":format!("Quoted @1+literal {n}: مرحبا 🚦 \"value\"\n").repeat(10)
            }))
            .collect::<Vec<_>>()
    ));
    r.commit("shared records");
    let side = r.linked("side");
    std::fs::write(side.join("unrelated.txt"), "Other branch revision").unwrap();
    git(&side, &["add", "unrelated.txt"]);
    git(
        &side,
        &["commit", "-q", "--no-gpg-sign", "-m", "other revision"],
    );
    r
}

fn checked(r: &Repo, args: &[&str]) -> Value {
    let out = r.run(args);
    assert!(out.status.success(), "{out:?}");
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn shared_bodies_keep_all_ordered_branch_occurrences_and_exact_source_pointers() {
    let r = shared_project();
    let full = checked(
        &r,
        &[
            "brief",
            "--branches",
            "main,side",
            "--incoming",
            "false",
            "--budget",
            "60000",
        ],
    );
    let compact = checked(
        &r,
        &[
            "brief",
            "--branches",
            "main,side",
            "--incoming",
            "false",
            "--view-id",
            full["freshness"]["view_id"].as_str().unwrap(),
            "--compact",
            "--budget",
            "60000",
        ],
    );
    assert_eq!(compact["memq"]["encoding"], "shared-v1");
    assert_eq!(compact["items"].as_array().unwrap().len(), 12);
    assert_eq!(compact["occurrences"].as_array().unwrap().len(), 24);
    assert_eq!(compact["sources"].as_array().unwrap().len(), 2);
    assert!(
        compact["budget"]["used"].as_u64().unwrap() * 2 < full["budget"]["used"].as_u64().unwrap()
    );
    let expanded = expanded_items(&compact);
    assert_eq!(expanded.len(), full["items"].as_array().unwrap().len());
    for (expected, actual) in full["items"].as_array().unwrap().iter().zip(&expanded) {
        for field in [
            "id",
            "text",
            "pointer",
            "kind",
            "native_status",
            "acceptance",
            "attribution",
            "section",
            "availability",
            "committed",
            "untrusted",
        ] {
            assert_eq!(actual[field], expected[field], "changed {field}");
        }
        for field in ["branch", "origin", "commit", "object_format", "dirty"] {
            assert_eq!(actual["observation"][field], expected["observation"][field]);
        }
        assert!(actual["id"].as_str().unwrap().starts_with("mq:"));
        assert!(actual["text"].as_str().unwrap().contains("@1+literal"));
    }
    assert!(compact["continuation"].is_null());
    assert_eq!(compact["incomplete"], false);
}

#[test]
fn both_mcp_transports_return_the_same_shared_payload_as_the_cli() {
    let r = shared_project();
    let saved = r.ok(&[
        "brief",
        "--branches",
        "main,side",
        "--incoming",
        "false",
        "--budget",
        "60000",
    ]);
    let view = saved["freshness"]["view_id"].as_str().unwrap();
    r.write(
        "later.txt",
        "Publish a later view before comparing transports",
    );
    r.commit("later view");
    r.ok(&[
        "brief",
        "--branches",
        "main,side",
        "--incoming",
        "false",
        "--budget",
        "60000",
    ]);
    let expected = checked(
        &r,
        &[
            "brief",
            "--branches",
            "main,side",
            "--incoming",
            "false",
            "--view-id",
            view,
            "--compact",
            "--budget",
            "60000",
        ],
    );
    assert_eq!(expected["memq"]["encoding"], "shared-v1");
    for fallback in [false, true] {
        let mut command = r.command();
        command.arg("mcp");
        if fallback {
            command.arg("--text-fallback");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let message = json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"brief","arguments":{
                "branches":["main","side"],"incoming":false,
                "view_id":view,"compact":true,"budget":60000
            }}
        });
        writeln!(child.stdin.take().unwrap(), "{message}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["result"]["isError"], false);
        let result = if fallback {
            let raw = response["result"]["content"][0]["text"].as_str().unwrap();
            support::budget_check(raw.as_bytes());
            serde_json::from_str(raw).unwrap()
        } else {
            let raw = serde_json::to_vec(&response["result"]["structuredContent"]).unwrap();
            support::budget_check(&raw);
            response["result"]["structuredContent"].clone()
        };
        assert_eq!(result, expected);
    }
}
