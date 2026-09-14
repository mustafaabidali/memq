mod support;

use serde_json::{Value, json};
use support::Repo;

fn removed_record(r: &Repo) -> (String, String, String) {
    r.record_source();
    r.records(json!([
        {"id":"original","text":"Old evidence مرحبا 🚦 ".repeat(350)},
        {"id":"anchor","text":"Retain the source"}
    ]));
    r.commit("original evidence");
    let first = r.ok(&["brief", "--budget", "40000"]);
    let original = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == "original")
        .unwrap();
    let id = original["id"].as_str().unwrap().to_owned();
    let text = original["text"].as_str().unwrap().to_owned();
    r.records(json!([{"id":"anchor","text":"Retain the source"}]));
    r.commit("remove original record");
    let absent = r.ok(&["brief"]);
    (
        id,
        text,
        absent["freshness"]["view_id"].as_str().unwrap().to_owned(),
    )
}

fn reintroduce(r: &Repo) {
    r.records(json!([
        {"id":"original","text":"FUTURE REPLACEMENT ".repeat(450)},
        {"id":"anchor","text":"Retain the source"}
    ]));
    r.commit("reintroduce with a different body");
    r.ok(&["brief", "--budget", "40000"]);
}

fn read(r: &Repo, args: &[&str]) -> Value {
    let out = r.run(args);
    assert!(out.status.success(), "{out:?}");
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn an_old_views_historical_evidence_does_not_advance_to_a_future_publication() {
    let r = Repo::initialized();
    let (id, text, absent) = removed_record(&r);
    let before = read(
        &r,
        &["show", &id, "--view-id", &absent, "--budget", "40000"],
    );
    assert_eq!(before["items"][0]["text"], text);
    reintroduce(&r);
    for compact in [false, true] {
        let mut args = vec!["show", &id, "--view-id", &absent, "--budget", "40000"];
        if compact {
            args.push("--compact");
        }
        let after = read(&r, &args);
        let item = &support::expanded_items(&after)[0];
        assert_eq!(item["text"], text);
        assert_eq!(
            item["evidence_view_id"],
            before["items"][0]["evidence_view_id"]
        );
        assert_eq!(item["availability"], "historical");
        assert_eq!(item["absent_since"], before["items"][0]["absent_since"]);
    }
}

#[test]
fn historical_continuation_keeps_its_original_unicode_body_after_new_publication() {
    for compact in [false, true] {
        let r = Repo::initialized();
        let (id, text, absent) = removed_record(&r);
        let mut base = vec![
            "show",
            &id,
            "--view-id",
            &absent,
            "--budget-kind",
            "bytes",
            "--budget",
            "4200",
        ];
        if compact {
            base.push("--compact");
        }
        let mut page = read(&r, &base);
        assert!(page["continuation"].is_string());
        reintroduce(&r);
        let mut recovered = String::new();
        let mut evidence_view = None;
        for _ in 0..50 {
            for item in support::expanded_items(&page) {
                assert_eq!(item["text_range"][0], recovered.len());
                recovered.push_str(item["text"].as_str().unwrap());
                assert_eq!(item["text_range"][1], recovered.len());
                if let Some(view) = &evidence_view {
                    assert_eq!(&item["evidence_view_id"], view);
                } else {
                    evidence_view = Some(item["evidence_view_id"].clone());
                }
                assert!(!recovered.contains("FUTURE REPLACEMENT"));
            }
            let Some(next) = page["continuation"].as_str().map(str::to_owned) else {
                assert_eq!(recovered, text);
                break;
            };
            let mut args = base.clone();
            args.extend(["--continuation", &next]);
            page = read(&r, &args);
        }
        assert_eq!(recovered, text, "all original evidence must be recovered");
    }
}
