mod support;

#[test]
fn stdout_itself_fits_an_exact_byte_limit_without_an_unaccounted_newline() {
    let r = support::Repo::initialized();
    let mut limit = 20000;
    for _ in 0..8 {
        let out = r.run(&[
            "brief",
            "--budget-kind",
            "bytes",
            "--budget",
            &limit.to_string(),
        ]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            out.stdout.len(),
            value["budget"]["used"].as_u64().unwrap() as usize
        );
        assert!(
            out.stdout.len() <= limit,
            "raw stdout exceeds the advertised byte bound"
        );
        if out.stdout.len() == limit {
            return;
        }
        limit = out.stdout.len();
    }
    panic!("fixture must reach an exact byte budget");
}
use serde_json::{Value, json};
use support::{Repo, budget_check};

#[test]
fn actual_unicode_payloads_obey_both_budget_units() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"دليل/طلب","status":"blocked","decider":"owner","text":"تسجيل الدخول بالعربية؛ ما زال محظوراً."}]));
    for tokenizer in ["o200k_base", "cl100k_base"] {
        let out = r.run(&["brief", "--budget", "4000", "--tokenizer", tokenizer]);
        assert!(out.status.success());
        budget_check(&out.stdout);
    }
    let out = r.run(&["brief", "--budget", "18000", "--budget-kind", "bytes"]);
    assert!(out.status.success());
    budget_check(&out.stdout);
    r.error(&["brief", "--budget", "1"], "budget_below_minimum");
}

#[test]
fn required_omissions_and_bound_continuations() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!((0..30).map(|n|json!({"id":format!("D-{n}"),"status":"blocked","decider":"owner","text":"required constraint ".repeat(20)})).collect::<Vec<_>>()));
    let out = r.run(&["brief", "--budget", "1800"]);
    assert!(out.status.success());
    budget_check(&out.stdout);
    let mut b: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(b["incomplete"], true);
    assert_eq!(b["reason"], "required_items_omitted");
    let mut omitted = b["omitted"].as_array().unwrap().len();
    while let Some(c) = b["continuation"].as_str() {
        let out = r.run(&["brief", "--budget", "1800", "--continuation", c]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        budget_check(&out.stdout);
        b = serde_json::from_slice(&out.stdout).unwrap();
        omitted += b["omitted"].as_array().unwrap().len();
    }
    assert!(omitted > 0);
    let initial = r.ok(&["brief", "--budget", "1800"]);
    r.error(
        &[
            "brief",
            "--task",
            "different",
            "--continuation",
            initial["continuation"].as_str().unwrap(),
        ],
        "stale_continuation",
    );
}
