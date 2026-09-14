mod support;
use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;
use support::Repo;

#[test]
fn cli_and_mcp_share_payload_and_four_tools() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"one","text":"quoted evidence"}]));
    let cli = r.ok(&["brief"]);
    let mut child = r
        .command()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"brief","arguments":{}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let rows: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let names: Vec<_> = rows[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["brief", "search", "show", "note"]);
    assert_eq!(rows[2]["result"]["structuredContent"], cli);
    assert!(rows[2]["result"]["content"].as_array().unwrap().is_empty());
    support::budget_check(
        serde_json::to_string(&rows[2]["result"]["structuredContent"])
            .unwrap()
            .as_bytes(),
    );
}

#[test]
fn text_only_clients_receive_one_payload_without_a_structured_output_contract() {
    let r = Repo::initialized();
    let cli = r.ok(&["brief"]);
    let mut child = r
        .command()
        .args(["mcp", "--text-fallback"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"brief","arguments":{}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let rows: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    for tool in rows[1]["result"]["tools"].as_array().unwrap() {
        assert!(
            tool.get("outputSchema").is_none(),
            "MCP clients require structuredContent when outputSchema is advertised"
        );
    }
    let result = &rows[2]["result"];
    assert!(result.get("structuredContent").is_none());
    assert_eq!(result["content"].as_array().unwrap().len(), 1);
    let text = result["content"][0]["text"].as_str().unwrap();
    support::budget_check(text.as_bytes());
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), cli);
}
