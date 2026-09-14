mod support;
use serde_json::json;
use support::Repo;

fn configured() -> Repo {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"candidate","text":"cobalt","depends_on":"endpoint"},
        {"id":"endpoint","text":"authored relationship evidence"},
        {"id":"unrelated","text":"ordinary words"}
    ]));
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/embedding.py");
    r.add_source(&format!(
        "[vectors]\ncommand=['python3',{}]\nmodel='test-plumbing-only'\ndims=4\npreprocessing_version='fixture-v1'\ntimeout_seconds=1\n",
        json!(script)
    ));
    r
}

#[test]
fn independent_vector_candidate_enters_without_lexical_match() {
    let r = configured();
    let before = r.ok(&["search", "quartz"]);
    assert!(before["items"].as_array().unwrap().is_empty());
    assert_eq!(r.ok(&["brief"])["coverage"]["vectors"], "pending");
    assert_eq!(r.ok(&["embed"])["published"], 3);
    assert_eq!(r.ok(&["brief"])["coverage"]["vectors"], "ready");
    let result = r.ok(&["search", "quartz", "--budget", "10000"]);
    let item = &result["items"][0];
    assert!(item["id"].as_str().unwrap().ends_with(":candidate"));
    assert_eq!(item["paths_found"], json!(["vector"]));
    assert!(
        item["relations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "semantic" && r["model"] == "test-plumbing-only")
    );
    assert_eq!(
        result["coverage"]["vector_details"]["scope"],
        "eligible_view_before_candidate_limit"
    );
}

#[test]
fn exact_priority_partial_timeout_version_validation_and_forgetting() {
    let r = configured();
    r.ok(&["embed"]);
    let exact = r.ok(&["search", "ordinary words", "--budget", "10000"]);
    assert_eq!(exact["items"][0]["paths_found"][0], "exact");
    let out = r
        .command()
        .env("MEMQ_TEST_EMBED_DELAY", "1")
        .args(["search", "cobalt timeout", "--budget", "10000"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let result: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        result["coverage"]["vector_details"]["error"],
        "embedding_timeout"
    );
    assert!(!result["items"].as_array().unwrap().is_empty());
    let candidate = r.ok(&["search", "cobalt"])["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    r.records(json!([{"id":"candidate","text":"changed evidence"}]));
    assert_eq!(r.ok(&["brief"])["coverage"]["vectors"], "pending");
    let changed = r.ok(&["search", "changed evidence"]);
    assert_eq!(changed["coverage"]["vector_details"]["indexed_versions"], 0);
    r.ok(&["forget", &candidate]);
    let found = r.ok(&["search", "quartz"]);
    assert!(
        found["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["id"] != candidate)
    );
}

#[test]
fn authored_links_expand_one_hop_and_keep_missing_endpoints() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"start","text":"needle","depends_on":["endpoint","missing"]},
        {"id":"endpoint","text":"supporting facts","depends_on":"third"},
        {"id":"third","text":"two hops"}
    ]));
    let s = r.ok(&["search", "needle", "--budget", "10000"]);
    assert_eq!(s["items"].as_array().unwrap().len(), 2);
    assert_eq!(s["items"][1]["paths_found"], json!(["authored"]));
    assert_eq!(s["coverage"]["expansion"]["missing_endpoints"], 1);
}
