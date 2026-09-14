mod support;
use serde_json::{Value, json};
use support::Repo;

#[test]
fn graph_coverage_has_independent_revision_and_file_freshness() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"callback","file":"callback.rs","text":"Callback evidence"}]));
    r.write("callback.rs", "fn callback() { exchange(); }");
    r.commit("graph input");
    let brief = r.ok(&["brief"]);
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let report = json!({
        "format":1,"tool":"codebase-memory-mcp","tool_version":"synthetic-v1",
        "report_id":"synthetic-coverage","project_id":brief["scope"]["project_id"],
        "worktree":memq::util::hash(repo.root.to_string_lossy().as_bytes()),
        "checked_revision":r.git(&["rev-parse","HEAD"]),"object_format":"sha1",
        "complete":true,
        "files":[{"path":"callback.rs","content_sha256":memq::util::hash(b"fn callback() { exchange(); }")}],
        "relations":[{"from":"callback.rs","to":"exchange","relationship":"calls"}]
    });
    let path = r.temp.path().join("code-report.json");
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let call = || {
        let out = r
            .command()
            .env("MEMQ_CODE_REPORT", &path)
            .arg("brief")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    let b = call();
    assert_eq!(b["coverage"]["code"]["status"], "current");
    let code = b["items"][0]["relations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == "code")
        .unwrap();
    assert_eq!(code["coverage_report_id"], "synthetic-coverage");
    assert_eq!(code["tool_version"], "synthetic-v1");
    r.write("callback.rs", "fn callback() { changed(); }");
    let b = call();
    assert_eq!(b["freshness"]["status"], "current");
    assert_eq!(b["coverage"]["code"]["status"], "outdated");
    assert_ne!(b["freshness"]["view_id"], brief["freshness"]["view_id"]);
    std::fs::write(&path, b"{broken").unwrap();
    let b = call();
    assert_eq!(b["coverage"]["code"]["status"], "unavailable");
    assert!(!b["items"].as_array().unwrap().is_empty());
}
