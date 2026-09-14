mod support;
use serde_json::json;
use support::Repo;

#[test]
fn records_brief_show_note_restart_and_source_edit() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"blocked-auth","status":"blocked","reason":"Provider readiness","decider":"owner","next_action":"Inspect callback"}]));
    r.commit("synthetic records");
    let b = r.ok(&["brief", "--budget", "5000"]);
    let id = b["items"][0]["id"].as_str().unwrap();
    let shown = r.ok(&["show", id, "--budget", "5000"]);
    assert!(
        shown["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Provider readiness")
    );
    let n = r.ok(&[
        "note",
        "--text",
        "Inspected recorded blocker",
        "--evidence",
        id,
        "--idempotency-key",
        "round-trip",
    ]);
    assert_eq!(n["durability"], "durable");
    assert_eq!(n["staging"], "staged");
    let restarted = r.ok(&["brief", "--budget", "8000"]);
    assert!(
        restarted["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == n["id"])
    );
    let same = r.ok(&["brief", "--budget", "8000"]);
    assert_eq!(
        restarted["freshness"]["view_id"],
        same["freshness"]["view_id"]
    );
    r.records(json!([{"id":"blocked-auth","status":"accepted","reason":"Provider readiness recorded","decider":"maintainer"}]));
    let changed = r.ok(&["brief", "--budget", "8000"]);
    assert_ne!(
        same["freshness"]["view_id"],
        changed["freshness"]["view_id"]
    );
    assert!(
        changed["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == id && i["acceptance"] == "accepted" && i["committed"] == false)
    );
}
