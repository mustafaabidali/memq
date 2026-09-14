mod support;
use serde_json::{Value, json};
use std::io::Write;
use std::process::Stdio;
use support::Repo;

fn hook(repo: &Repo, event: &str) -> Value {
    hook_mode(repo, event, false)
}

fn hook_mode(repo: &Repo, event: &str, compact: bool) -> Value {
    let mut command = repo.command();
    command.args(["hook", "--budget", "2200"]);
    if compact {
        command.arg("--compact");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            &serde_json::to_vec(&json!({"hook_event_name":event,"session_id":"synthetic-session"}))
                .unwrap(),
        )
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn compact_hook_is_opt_in_and_its_actual_context_obeys_the_budget() {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{"id":"pending","text":"Keep the approval gate","status":"blocked"}]));
    for event in ["SessionStart", "UserPromptSubmit"] {
        let full = hook_mode(&r, event, false);
        let compact = hook_mode(&r, event, true);
        let full: Value = serde_json::from_str(
            full["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let text = compact["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        let brief: Value = serde_json::from_str(text).unwrap();
        assert!(full["memq"].get("representation").is_none());
        assert_eq!(brief["memq"]["representation"], "compact");
        let items = support::expanded_items(&brief);
        assert_eq!(items[0]["id"], full["items"][0]["id"]);
        assert_eq!(items[0]["pointer"], full["items"][0]["pointer"]);
        assert_eq!(items[0]["text"], full["items"][0]["text"]);
        let actual = tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(text)
            .len();
        assert_eq!(brief["budget"]["used"], actual);
        assert!(actual <= 2200);
        assert!(brief["budget"]["used"].as_u64() < full["budget"]["used"].as_u64());
    }
    assert_eq!(hook_mode(&r, "PostCompact", true), json!({}));
}

#[test]
fn startup_and_post_compaction_prompt_deliver_one_bounded_envelope() {
    let r = Repo::initialized();
    r.record_source();
    r.records(
        json!([{"id":"pending","text":"Inspect callback","status":"blocked","decider":"owner"}]),
    );
    let start = hook(&r, "SessionStart");
    let context = start["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    let brief: Value = serde_json::from_str(context).unwrap();
    assert!(!brief["freshness"]["view_id"].as_str().unwrap().is_empty());
    let bpe = tiktoken_rs::o200k_base().unwrap();
    assert!(bpe.encode_with_special_tokens(context).len() <= 2200);
    assert_eq!(
        bpe.encode_with_special_tokens(context).len(),
        brief["budget"]["used"].as_u64().unwrap() as usize
    );
    r.records(json!([{"id":"pending","text":"Provider remains blocked","status":"blocked","decider":"owner"}]));
    assert_eq!(hook(&r, "PostCompact"), json!({}));
    let next = hook(&r, "UserPromptSubmit");
    let next: Value = serde_json::from_str(
        next["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_ne!(brief["freshness"]["view_id"], next["freshness"]["view_id"]);
    assert!(
        next["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Provider remains blocked")
    );
}

#[test]
fn codex_hook_template_has_only_supported_command_events() {
    let template: Value =
        serde_json::from_str(include_str!("../examples/harness/codex-hooks.json")).unwrap();
    let hooks = template["hooks"].as_object().unwrap();
    assert_eq!(hooks.len(), 3);
    for event in ["SessionStart", "UserPromptSubmit", "PostCompact"] {
        assert_eq!(hooks[event][0]["hooks"][0]["type"], "command");
        assert!(
            hooks[event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("memq hook")
        );
    }
}
