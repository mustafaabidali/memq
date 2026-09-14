mod support;

use serde_json::json;
use support::Repo;

#[test]
fn an_omission_page_does_not_claim_optional_evidence_is_required() {
    let r = Repo::initialized();
    r.add_source(
        r#"[[source]]
id = "plain"
kind = "json-records"
path = "records.json"
collection = "records"
id_field = "id"
"#,
    );
    r.records(json!(
        (0..40)
            .map(|n| json!({
                "id": format!("evidence-{n}"),
                "text":"Optional supporting evidence"
            }))
            .collect::<Vec<_>>()
    ));
    let first = r.ok(&["brief", "--budget", "1400"]);
    assert_eq!(first["reason"], "results_omitted");
    let continuation = first["continuation"].as_str().unwrap();
    let second = r.ok(&["brief", "--budget", "1400", "--continuation", continuation]);
    assert_eq!(second["reason"], "results_omitted");
    assert_eq!(second["incomplete"], true);
}
