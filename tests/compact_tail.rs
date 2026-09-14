mod support;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::fs;
use support::{Repo, expanded_items};

fn measured(value: &Value, bytes: bool) -> usize {
    let raw = serde_json::to_string(value).unwrap();
    let count = if bytes {
        raw.len()
    } else {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(&raw)
            .len()
    };
    assert_eq!(value["budget"]["used"], count);
    assert!(count <= value["budget"]["limit"].as_u64().unwrap() as usize);
    count
}

fn complete_tail(bytes: bool) {
    let r = Repo::initialized();
    r.record_source();
    let mut records = vec![json!({"id":"000","text":"synthetic ".repeat(6_000)})];
    records.extend(
        (0..20).map(|n| {
            json!({"id":format!("{:03}", n + 100),"text":format!("Synthetic final evidence {n} مرحبا")})
        }),
    );
    r.records(json!(records));
    let kind = if bytes { "bytes" } else { "tokens" };
    let small = if bytes { "4500" } else { "1500" };
    let mut page = r.ok(&[
        "brief",
        "--compact",
        "--budget-kind",
        kind,
        "--budget",
        small,
    ]);
    let view = page["freshness"]["view_id"].as_str().unwrap().to_owned();
    for _ in 0..60 {
        measured(&page, bytes);
        let cursor = page["continuation"]
            .as_str()
            .expect("the long first record needs several pages")
            .to_owned();
        // Inspect a cursor returned by the executable; never forge one.
        let cursor_data: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&cursor).unwrap()).unwrap();
        let offset = cursor_data["offset"].as_u64().unwrap();
        if offset > 30_000 {
            let complete = r.ok(&[
                "brief",
                "--compact",
                "--view-id",
                &view,
                "--continuation",
                &cursor,
                "--budget-kind",
                kind,
                "--budget",
                "300000",
            ]);
            let needed = measured(&complete, bytes) + 8;
            let expected = expanded_items(&complete);
            assert!(complete["continuation"].is_null());
            // Previously served bytes must not disable the complete-tail
            // attempt. Keep enough final records to expose cursor overhead.
            if 60_000 > needed * 8 && expected.len() >= 3 {
                let tight = r.ok(&[
                    "brief",
                    "--compact",
                    "--view-id",
                    &view,
                    "--continuation",
                    &cursor,
                    "--budget-kind",
                    kind,
                    "--budget",
                    &needed.to_string(),
                ]);
                measured(&tight, bytes);
                assert!(
                    tight["continuation"].is_null(),
                    "the complete remaining evidence fits: {tight}"
                );
                assert_eq!(expanded_items(&tight), expected);
                assert_eq!(tight["omitted_count"], 0);
                return;
            }
        }
        page = r.ok(&[
            "brief",
            "--compact",
            "--view-id",
            &view,
            "--continuation",
            &cursor,
            "--budget-kind",
            kind,
            "--budget",
            small,
        ]);
    }
    panic!("failed to reach a bounded final remainder");
}

#[test]
fn a_compact_tail_finishes_when_its_complete_byte_response_fits() {
    complete_tail(true);
}

#[test]
fn a_compact_tail_finishes_when_its_complete_token_response_fits() {
    complete_tail(false);
}

fn shared_tail(bytes: bool) {
    let r = Repo::initialized();
    r.record_source();
    r.records(json!([{
        "id":"large","status":"blocked","text":"Synthetic shared evidence ".repeat(480)
    }]));
    r.commit("Synthetic shared evidence");
    let branches: Vec<_> = (0..24).map(|n| format!("copy-{n:02}")).collect();
    for branch in &branches {
        r.git(&["branch", branch, "HEAD"]);
    }
    let sessions = r.temp.path().join("sessions");
    fs::create_dir(&sessions).unwrap();
    let mut journal = format!(
        "{}\n",
        json!({
            "type":"session","version":3,"id":"short-tail-session",
            "cwd":r.root,"branch":branches[0]
        })
    );
    for n in 0..20 {
        journal.push_str(&format!(
            "{}\n",
            json!({
                "type":"message","id":format!("tail-{n:02}"),
                "message":{"content":format!("Synthetic short final evidence {n} مرحبا")}
            })
        ));
    }
    fs::write(sessions.join("tail.jsonl"), journal).unwrap();
    let names = branches.join(",");
    let kind = if bytes { "bytes" } else { "tokens" };
    let call = |limit: usize, view: Option<&str>| {
        let mut command = r.command();
        command
            .env("MEMQ_OMP_STORE", &sessions)
            .env("MEMQ_CODEX_STORE", "")
            .env("MEMQ_OPENCODE_STORE", "")
            .env_remove("MEMQ_CODE_REPORT")
            .args([
                "brief",
                "--branches",
                &names,
                "--compact",
                "--budget-kind",
                kind,
                "--budget",
                &limit.to_string(),
            ]);
        if let Some(view) = view {
            command.args(["--view-id", view]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let value = serde_json::from_slice::<Value>(&output.stdout).unwrap();
        measured(&value, bytes);
        value
    };
    let complete = call(2_000_000, None);
    assert!(complete["continuation"].is_null());
    assert_eq!(complete["memq"]["encoding"], "shared-v1");
    let expected = expanded_items(&complete);
    assert_eq!(expected.len(), 44);
    let limit = measured(&complete, bytes) + 8;
    let duplicated_bytes: usize = expected
        .iter()
        .map(|item| item["text"].as_str().unwrap().len() + 1)
        .sum();
    assert!(duplicated_bytes > limit * 8);
    let tight = call(limit, complete["freshness"]["view_id"].as_str());
    assert!(
        tight["continuation"].is_null(),
        "shared complete evidence fits in {limit} units: {tight}"
    );
    assert_eq!(tight["omitted_count"], 0);
    assert_eq!(expanded_items(&tight), expected);
}

#[test]
fn shared_bodies_and_short_tail_finish_at_the_complete_byte_budget() {
    shared_tail(true);
}

#[test]
fn shared_bodies_and_short_tail_finish_at_the_complete_token_budget() {
    shared_tail(false);
}
