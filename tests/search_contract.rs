mod support;
use serde_json::json;
use support::Repo;

#[test]
fn literal_exact_arabic_and_engine_errors() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([
        {"id":"D-055","text":"Use auth/callback.ts for callback"},
        {"id":"arabic","text":"تَسْجِيل الدُّخُول"}
    ]));
    let s = r.ok(&["search", "D-055"]);
    assert!(s["items"][0]["id"].as_str().unwrap().ends_with("D-055"));
    assert_eq!(s["items"][0]["paths_found"][0], "exact");
    let s = r.ok(&["search", "تسجيل الدخول"]);
    assert!(!s["items"].as_array().unwrap().is_empty());
    r.ok(&["search", "\" OR NOT * ("]);
    let out = r
        .command()
        .env("MEMQ_FAULT", "search_engine")
        .args(["search", "uncached-query"])
        .output()
        .unwrap();
    assert!(!out.status.success());
}
