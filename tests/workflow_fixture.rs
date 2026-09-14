mod support;
use serde_json::{Value, json};
use support::Repo;

fn fixture() -> Repo {
    let r = Repo::initialized();
    let project = r.ok(&["brief"])["scope"]["project_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/facebook");
    for e in walkdir::WalkDir::new(&root) {
        let e = e.unwrap();
        if e.file_type().is_file() && e.file_name() != "questions.jsonl" {
            r.write(
                e.path().strip_prefix(&root).unwrap().to_str().unwrap(),
                std::fs::read(e.path()).unwrap(),
            );
        }
    }
    r.write(
        ".memq/config.toml",
        include_str!("fixtures/config/valid-project.toml")
            .replace("01ARZ3NDEKTSV4RRFFQ69G5FAV", &project),
    );
    r.commit("synthetic workflow");
    r
}

#[test]
fn five_registries_and_full_facebook_questions_retain_evidence() {
    let r = fixture();
    let b = r.ok(&["brief", "--budget", "20000"]);
    assert_eq!(b["coverage"]["sources_read"], 9);
    for source in [
        "manifest-next",
        "manifest-specs",
        "manifest-adrs",
        "manifest-plans",
        "manifest-reviews",
    ] {
        assert!(
            b["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["source_id"] == source)
        );
    }
    for line in include_str!("fixtures/facebook/questions.jsonl").lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        let mut found = std::collections::BTreeSet::new();
        for query in case["queries"].as_array().unwrap() {
            let result = r.ok(&["search", query.as_str().unwrap(), "--budget", "20000"]);
            for item in result["items"].as_array().unwrap() {
                found.insert(item["native_id"].as_str().unwrap().to_owned());
            }
        }
        for id in case["expected_record_ids"].as_array().unwrap() {
            assert!(
                found.contains(id.as_str().unwrap()),
                "missing {} for {}",
                id,
                case["id"]
            );
        }
        for path in case["expected_source_paths"].as_array().unwrap() {
            assert!(r.root.join(path.as_str().unwrap()).is_file());
        }
    }
    let gap = r.ok(&["search", "verification-gap", "--budget", "6000"]);
    assert!(
        gap["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("No live Facebook")
    );
    assert!(gap["items"][0].get("verification").is_none());
    assert_eq!(gap["items"][0]["native_status"], "open");
    let plan = r.ok(&["search", "facebook-plan", "--budget", "10000"]);
    assert_eq!(
        plan["items"][0]["native_status"],
        "authored_awaiting_founder_signoff"
    );
    assert_eq!(plan["items"][0]["acceptance"], "proposed");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(r.root.join("specs/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["next"][0]["handoff"], json!(null));
}
