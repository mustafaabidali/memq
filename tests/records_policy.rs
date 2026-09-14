mod support;
use serde_json::json;
use support::Repo;

#[test]
fn policy_preserves_approval_and_requires_authority() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"D-old","status":"accepted","decider":"owner","reason":"Keep compatibility"},
        {"id":"D-new","status":"proposed","decider":"owner","supersedes":"D-old","reason":"An alternative"},
        {"id":"D-missing","status":"accepted"},
        {"id":"D-other","status":"accepted","decider":"contributor"}
    ]));
    let b = r.ok(&["brief", "--budget", "12000"]);
    let items = b["items"].as_array().unwrap();
    assert_eq!(
        items
            .iter()
            .find(|v| v["id"].as_str().unwrap().ends_with("D-old"))
            .unwrap()["acceptance"],
        "accepted"
    );
    assert_eq!(
        items
            .iter()
            .find(|v| v["id"].as_str().unwrap().ends_with("D-missing"))
            .unwrap()["acceptance"],
        "unverified"
    );
    assert_eq!(
        items
            .iter()
            .find(|v| v["id"].as_str().unwrap().ends_with("D-other"))
            .unwrap()["acceptance"],
        "unverified"
    );
    assert!(
        items
            .iter()
            .find(|v| v["id"].as_str().unwrap().ends_with("D-old"))
            .unwrap()["supersession"]
            .is_null()
    );
    r.records(json!([
        {"id":"D-old","status":"accepted","decider":"owner","reason":"Keep compatibility"},
        {"id":"D-new","status":"accepted","decider":"maintainer","supersedes":"D-old","reason":"Compatibility is preserved"}
    ]));
    let b = r.ok(&["brief", "--budget", "12000"]);
    assert_eq!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"].as_str().unwrap().ends_with("D-old"))
            .unwrap()["section"],
        "superseded"
    );
}

#[test]
fn redaction_precedes_sqlite_and_note_writes() {
    let r = Repo::initialized();
    r.record_source();
    let secret = format!("sk-proj-{}", "aB7xZ9".repeat(9));
    r.records(json!([{"id":"evidence","status":"pending","api_key":secret,"text":format!("Bearer {secret}")}]));
    r.ok(&["brief"]);
    r.ok(&[
        "note",
        "--text",
        &format!("api_key={secret}"),
        "--idempotency-key",
        "redact",
    ]);
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let rows: String = db
        .query_row(
            "SELECT group_concat(payload_json,'') FROM item_versions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!rows.contains(&secret));
    assert!(rows.contains("[REDACTED]"));
    for file in std::fs::read_dir(r.root.join(".memq/notes")).unwrap() {
        assert!(
            !std::fs::read_to_string(file.unwrap().path())
                .unwrap()
                .contains(&secret)
        );
    }
    let opaque = "aZ9mQ2vR8xL4nT7pK5cD1wS6uF3hJ0bE";
    let hash = memq::util::hash(opaque);
    r.records(json!([{"id":"opaque","text":format!("{opaque} password=x {hash}")}]));
    r.ok(&["brief"]);
    let rows: String = db
        .query_row(
            "SELECT group_concat(payload_json,'') FROM item_versions",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!rows.contains(opaque));
    assert!(!rows.contains("password=x"));
    assert!(
        rows.contains(&hash),
        "content hashes are not redacted as secrets"
    );
}

#[test]
fn duplicate_markdown_headings_are_ambiguous() {
    let source = memq::config::Source {
        id: "design".into(),
        name: "".into(),
        kind: "markdown".into(),
        path: "design.md".into(),
        collection: None,
        id_field: None,
        id_column: None,
        references: vec![],
        policy: Default::default(),
    };
    let p = memq::records::parse(
        &source,
        "design.md",
        b"# A\n## Blocker\none\n## Blocker\ntwo\n",
    )
    .unwrap();
    assert_eq!(
        p[0].payload.sections.iter().filter(|s| s.ambiguous).count(),
        2
    );
}

#[test]
fn ambiguous_native_references_do_not_supersede_and_qualified_conflicts_match() {
    let r = Repo::initialized();
    r.record_source();
    r.add_source(
        r#"[[source]]
id = "other"
kind = "json-records"
path = "other.json"
collection = "records"
id_field = "id"
[source.policy]
status_field = "status"
accepted_values = ["accepted"]
"#,
    );
    r.records(json!([
        {"id":"old","status":"accepted","decider":"owner"},
        {"id":"new","status":"accepted","decider":"owner","supersedes":"old"}
    ]));
    r.write(
        "other.json",
        r#"{"records":[{"id":"old","status":"accepted"}]}"#,
    );
    let b = r.ok(&["brief", "--budget", "12000"]);
    for old in b["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["native_id"] == "old")
    {
        assert!(old["supersession"].is_null());
    }
    let new = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["native_id"] == "new")
        .unwrap();
    assert_eq!(new["relations"][0]["status"], "ambiguous_endpoint");
    let qualified = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["source_id"] == "records" && i["native_id"] == "old")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    r.records(json!([
        {"id":"old","status":"accepted","decider":"owner"},
        {"id":"new","status":"accepted","decider":"owner","supersedes":qualified},
        {"id":"alternative","status":"accepted","decider":"owner","supersedes":qualified}
    ]));
    let b = r.ok(&["brief", "--budget", "12000"]);
    for alternative in b["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["native_id"] != "old")
    {
        assert!(
            alternative["flags"]
                .as_array()
                .unwrap()
                .contains(&json!("conflict"))
        );
    }
    let old = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["source_id"] == "records" && i["native_id"] == "old")
        .unwrap();
    assert!(
        old["supersession"].is_null(),
        "competing accepted records must not select an arbitrary winner"
    );
    assert!(
        old["flags"]
            .as_array()
            .unwrap()
            .contains(&json!("supersession_conflict"))
    );
}
