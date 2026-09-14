mod support;

use serde_json::{Value, json};
use support::Repo;

fn check_budget(value: &Value, limit: usize, bytes: bool, cl100k: bool) {
    let raw = serde_json::to_string(value).unwrap();
    let used = if bytes {
        raw.len()
    } else if cl100k {
        tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(&raw)
            .len()
    } else {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&raw)
            .len()
    };
    assert_eq!(value["budget"]["used"], used);
    assert!(used <= limit, "{used} > {limit}");
}

#[test]
fn a_large_commit_leaves_room_for_a_briefing_and_keeps_the_full_saved_change_set() {
    let r = Repo::initialized();
    r.records(json!([{"id":"progress","text":"Keep the callback review pending."}]));
    r.record_source();
    r.commit("configure synthetic memory");
    r.ok(&["brief", "--budget", "4000"]);
    for n in 0..240 {
        r.write(
            &format!("src/changed/{n:04}-synthetic-component-with-a-descriptive-name.txt"),
            "Synthetic code change\n",
        );
    }
    r.commit("many synthetic file changes");

    for compact in [false, true] {
        let mut args = vec!["brief", "--budget", "2000"];
        if compact {
            args.push("--compact");
        }
        let reply = r.ok(&args);
        check_budget(&reply, 2000, false, false);
        assert_eq!(reply["changes_since"]["paths_count"], 240);
        assert_eq!(reply["changes_since"]["paths_truncated"], true);
        assert!(reply["changes_since"]["paths"].as_array().unwrap().len() <= 20);
        assert_eq!(reply["incomplete"], true);
        assert!(!reply["items"].as_array().unwrap().is_empty(), "{reply}");

        let db = rusqlite::Connection::open(r.db_path()).unwrap();
        let meta: String = db
            .query_row(
                "SELECT meta_json FROM views WHERE view_id=?1",
                [reply["freshness"]["view_id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        let meta: Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(
            meta["changes_since"]["paths"].as_array().unwrap().len(),
            240
        );
        assert!(meta["changes_since"].get("paths_truncated").is_none());
    }
}

#[test]
fn long_unicode_paths_do_not_raise_the_envelope_minimum_or_break_other_reads() {
    let r = Repo::initialized();
    r.records(json!([{"id":"progress","text":"Inspect callback locale and session handling."}]));
    r.record_source();
    r.commit("configure synthetic memory");
    r.ok(&["brief", "--budget", "4000"]);
    let segment = "تغيير-واجهة-تسجيل-الدخول-".repeat(3);
    for n in 0..80 {
        r.write(
            &format!("src/{segment}/{segment}/{n:04}-callback.txt"),
            "Synthetic locale file\n",
        );
    }
    r.commit("long multilingual file changes");
    let brief = r.ok(&["brief", "--compact", "--budget", "2000"]);
    let view = brief["freshness"]["view_id"].as_str().unwrap();
    let project: toml::Value =
        toml::from_str(&std::fs::read_to_string(r.root.join(".memq/config.toml")).unwrap())
            .unwrap();
    let id = format!(
        "mq:{}:records:progress",
        project["project_id"].as_str().unwrap()
    );

    for args in [
        vec!["brief", "--view-id", view],
        vec![
            "search",
            "callback",
            "--view-id",
            view,
            "--incoming",
            "true",
        ],
        vec!["show", &id, "--view-id", view],
    ] {
        for compact in [false, true] {
            for (kind, limit, tokenizer) in [
                ("tokens", "2000", "o200k_base"),
                ("tokens", "2000", "cl100k_base"),
                ("bytes", "9000", "o200k_base"),
            ] {
                let mut request = args.clone();
                request.extend([
                    "--budget-kind",
                    kind,
                    "--budget",
                    limit,
                    "--tokenizer",
                    tokenizer,
                ]);
                if compact {
                    request.push("--compact");
                }
                let reply = r.ok(&request);
                check_budget(
                    &reply,
                    limit.parse().unwrap(),
                    kind == "bytes",
                    tokenizer == "cl100k_base",
                );
                assert_eq!(reply["changes_since"]["paths_count"], 80);
                assert_eq!(reply["changes_since"]["paths_truncated"], true);
                assert_eq!(reply["freshness"]["view_id"], view);
                assert_eq!(reply["incomplete"], true);
                assert!(!reply["items"].as_array().unwrap().is_empty(), "{reply}");
                for path in reply["changes_since"]["paths"].as_array().unwrap() {
                    assert!(path.as_str().unwrap().ends_with("-callback.txt"));
                }
            }
        }
    }
}
