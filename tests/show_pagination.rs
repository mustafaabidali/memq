mod support;

use memq::budget::Continuation;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use support::{Repo, git};

fn project(records: Value) -> Repo {
    let r = Repo::initialized();
    r.record_source();
    r.records(records);
    r.commit("synthetic show records");
    r
}

fn id<'a>(brief: &'a Value, native: &str) -> &'a str {
    brief["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["native_id"] == native)
        .unwrap()["id"]
        .as_str()
        .unwrap()
}

fn counts(r: &Repo, ids: &[&str]) -> Vec<i64> {
    let db = rusqlite::Connection::open(r.store_root().join("access.sqlite")).unwrap();
    ids.iter()
        .map(|id| {
            db.query_row("SELECT count FROM frecency WHERE item_id=?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .unwrap()
            .unwrap_or(0)
        })
        .collect()
}

fn checked_page(r: &Repo, args: &[&str]) -> Value {
    let out = r.run(args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    let raw = std::str::from_utf8(&out.stdout).unwrap();
    let actual = if value["budget"]["kind"] == "bytes" {
        out.stdout.len()
    } else if value["budget"]["encoding"] == "cl100k_base" {
        tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(raw)
            .len()
    } else {
        tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(raw)
            .len()
    };
    assert_eq!(value["budget"]["used"], actual);
    assert!(actual <= value["budget"]["limit"].as_u64().unwrap() as usize);
    value
}

#[test]
fn packs_short_requested_records_in_order_and_touches_each_logical_id_once() {
    let r = project(json!([
        {"id":"alpha","text":"First short record"},
        {"id":"beta","text":"Second short record"},
        {"id":"gamma","text":"Third short record"}
    ]));
    let brief = r.ok(&["brief", "--budget", "16000"]);
    let ids = [id(&brief, "gamma"), id(&brief, "alpha"), id(&brief, "beta")];
    for tokenizer in ["o200k_base", "cl100k_base"] {
        let before = counts(&r, &ids);
        let shown = checked_page(
            &r,
            &[
                "show",
                ids[0],
                ids[1],
                ids[2],
                ids[0],
                "--view-id",
                brief["freshness"]["view_id"].as_str().unwrap(),
                "--budget",
                "16000",
                "--tokenizer",
                tokenizer,
            ],
        );
        let items = shown["items"].as_array().unwrap();
        assert_eq!(
            items
                .iter()
                .map(|item| item["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [ids[0], ids[1], ids[2], ids[0]]
        );
        assert!(items.iter().all(|item| item["complete"] == true));
        assert!(shown["continuation"].is_null());
        assert_eq!(shown["coverage"]["truncated"], false);
        assert_eq!(shown["incomplete"], false);
        assert_eq!(shown["reason"], brief["freshness"]["reason"]);
        assert_eq!(
            counts(&r, &ids),
            before.iter().map(|n| n + 1).collect::<Vec<_>>()
        );
    }
}

#[test]
fn packing_keeps_every_branch_and_incoming_member_with_its_original_metadata() {
    let r = project(json!([
        {"id":"decision","status":"accepted","decider":"owner","reason":"First mode"},
        {"id":"constraint","status":"blocked","text":"Keep the approval gate"}
    ]));
    let side = r.linked("side");
    std::fs::write(
        side.join("records.json"),
        serde_json::to_vec(&json!({"records":[
            {"id":"decision","status":"accepted","decider":"owner","reason":"Second mode"},
            {"id":"constraint","status":"blocked","text":"Keep the approval gate"}
        ]}))
        .unwrap(),
    )
    .unwrap();
    git(&side, &["add", "records.json"]);
    git(
        &side,
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "synthetic side decision",
        ],
    );
    let remote = r.bare_remote();
    git(
        &remote,
        &[
            "fetch",
            "-q",
            side.to_str().unwrap(),
            "HEAD:refs/heads/main",
        ],
    );
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    r.add_source("[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=2\nmin_interval_seconds=300\n");

    let brief = r.ok(&["brief", "--branches", "main,side", "--budget", "30000"]);
    let ids = [id(&brief, "constraint"), id(&brief, "decision")];
    let expected: Vec<_> = ids
        .iter()
        .flat_map(|id| {
            r.ok(&[
                "show",
                id,
                "--branches",
                "main,side",
                "--incoming",
                "true",
                "--view-id",
                brief["freshness"]["view_id"].as_str().unwrap(),
                "--budget",
                "30000",
            ])["items"]
                .as_array()
                .unwrap()
                .clone()
        })
        .collect();
    assert!(
        expected.len() >= 6,
        "fixture must exercise all three scopes"
    );
    assert!(
        expected
            .iter()
            .any(|item| item["observation"]["origin"] == "incoming")
    );
    assert!(
        expected
            .iter()
            .any(|item| item["observation"]["branch"] == "side")
    );
    let before = counts(&r, &ids);
    let shown = checked_page(
        &r,
        &[
            "show",
            ids[0],
            ids[1],
            "--branches",
            "main,side",
            "--incoming",
            "true",
            "--view-id",
            brief["freshness"]["view_id"].as_str().unwrap(),
            "--budget",
            "30000",
        ],
    );
    let items = shown["items"].as_array().unwrap();
    assert_eq!(items.len(), expected.len());
    for (item, expected) in items.iter().zip(expected) {
        assert_eq!(*item, expected);
        let original = brief["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["observation"] == item["observation"])
            .unwrap();
        for (key, value) in original.as_object().unwrap() {
            assert_eq!(item[key], *value, "changed {key}");
        }
    }
    assert!(shown["continuation"].is_null());
    assert_eq!(
        counts(&r, &ids),
        before.iter().map(|n| n + 1).collect::<Vec<_>>()
    );
}

#[test]
fn mixed_full_and_partial_pages_keep_utf8_evidence_view_order_and_access_counts() {
    let r = project(json!([
        {"id":"alpha","text":"Short first record"},
        {"id":"beta","text":"مرحبا 🚦 quoted \"text\"\n".repeat(600)},
        {"id":"gamma","text":"Short last record"}
    ]));
    let brief = r.ok(&["brief", "--budget", "50000"]);
    let ids = [id(&brief, "alpha"), id(&brief, "beta"), id(&brief, "gamma")];
    let view = brief["freshness"]["view_id"].as_str().unwrap();
    let first = r.ok(&[
        "show",
        ids[0],
        "--view-id",
        view,
        "--budget-kind",
        "bytes",
        "--budget",
        "100000",
    ]);
    let middle = r.ok(&[
        "show",
        ids[1],
        "--view-id",
        view,
        "--budget-kind",
        "bytes",
        "--budget",
        "100000",
    ]);
    let last = r.ok(&[
        "show",
        ids[2],
        "--view-id",
        view,
        "--budget-kind",
        "bytes",
        "--budget",
        "100000",
    ]);
    let originals = [&first["items"][0], &middle["items"][0], &last["items"][0]];
    let mut middle_metadata = originals[1].clone();
    middle_metadata["text"] = json!("");
    let limit = (serde_json::to_vec(&first).unwrap().len()
        + serde_json::to_vec(&middle_metadata).unwrap().len()
        + 800)
        .to_string();
    let base = [
        "show",
        ids[0],
        ids[1],
        ids[2],
        "--view-id",
        view,
        "--budget-kind",
        "bytes",
        "--budget",
        &limit,
    ];
    let mut continuation: Option<String> = None;
    let mut reconstructed = [String::new(), String::new(), String::new()];
    let mut selected = 0;
    let mut pages = 0;
    loop {
        let mut args = base.to_vec();
        if let Some(next) = continuation.as_deref() {
            args.extend(["--continuation", next]);
        }
        let before = counts(&r, &ids);
        let page = checked_page(&r, &args);
        pages += 1;
        assert!(pages < 100, "pagination must make progress");
        let items = page["items"].as_array().unwrap();
        assert!(!items.is_empty());
        if pages == 1 {
            assert_eq!(
                items.len(),
                2,
                "pack the full first record and part of the second"
            );
            assert_eq!(items[0]["complete"], true);
            assert_eq!(items[1]["complete"], false);
        } else {
            assert_eq!(page["freshness"]["status"], "stale");
            assert_eq!(page["freshness"]["reason"], "retained_view");
        }
        assert_eq!(page["freshness"]["view_id"], view);
        let served: BTreeSet<_> = items
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect();
        let after = counts(&r, &ids);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(after[i], before[i] + i64::from(served.contains(id)));
        }
        for item in items {
            assert_eq!(item["id"], ids[selected]);
            let text = item["text"].as_str().unwrap();
            assert!(!text.is_empty(), "partial items must make byte progress");
            assert_eq!(item["text_range"][0], reconstructed[selected].len());
            reconstructed[selected].push_str(text);
            assert_eq!(item["text_range"][1], reconstructed[selected].len());
            assert_eq!(item["total_bytes"], originals[selected]["total_bytes"]);
            for key in [
                "version_id",
                "pointer",
                "observation",
                "relations",
                "acceptance",
            ] {
                assert_eq!(item[key], originals[selected][key], "changed {key}");
            }
            if item["complete"] == true {
                assert_eq!(
                    reconstructed[selected],
                    originals[selected]["text"].as_str().unwrap()
                );
                selected += 1;
            }
        }
        let Some(next) = page["continuation"].as_str() else {
            assert_eq!(selected, 3);
            assert_eq!(page["coverage"]["truncated"], false);
            assert_eq!(
                page["incomplete"], true,
                "retained view stays stale on the last page"
            );
            assert_eq!(page["reason"], "retained_view");
            break;
        };
        assert_eq!(page["reason"], "evidence_page");
        if pages == 1 {
            r.error(
                &["show", ids[1], ids[0], ids[2], "--continuation", next],
                "stale_continuation",
            );
            let mut invalid = Continuation::decode(next).unwrap();
            invalid.offset = originals[0]["text"].as_str().unwrap().len()
                + 1
                + originals[1]["text"].as_str().unwrap().find('م').unwrap()
                + 1;
            r.error(
                &[
                    "show",
                    ids[0],
                    ids[1],
                    ids[2],
                    "--continuation",
                    &invalid.encode().unwrap(),
                ],
                "stale_continuation",
            );
            assert_eq!(
                counts(&r, &ids),
                after,
                "failed pages must not record an access"
            );
            r.records(json!([
                {"id":"alpha","text":"Changed first record"},
                {"id":"beta","text":"Replacement evidence"},
                {"id":"gamma","text":"Changed last record"}
            ]));
            let current = r.ok(&["brief", "--budget", "50000"]);
            assert_ne!(current["freshness"]["view_id"], view);
        }
        continuation = Some(next.to_owned());
    }
    assert!(pages > 1, "fixture must exercise partial pagination");
}

#[test]
fn packs_forgotten_and_missing_placeholders_and_rejects_a_pre_forget_cursor() {
    let r = project(json!([
        {"id":"alpha","text":"Original evidence ".repeat(400)},
        {"id":"beta","text":"Keep this short record"}
    ]));
    let brief = r.ok(&["brief", "--budget", "16000"]);
    let ids = [id(&brief, "alpha"), id(&brief, "beta")];
    let view = brief["freshness"]["view_id"].as_str().unwrap();
    let paged = checked_page(
        &r,
        &[
            "show",
            ids[0],
            ids[1],
            "--view-id",
            view,
            "--budget-kind",
            "bytes",
            "--budget",
            "4500",
        ],
    );
    let old_cursor = paged["continuation"]
        .as_str()
        .expect("fixture must paginate");
    r.ok(&["forget", ids[0]]);
    r.error(
        &["show", ids[0], ids[1], "--continuation", old_cursor],
        "stale_continuation",
    );
    let unknown = format!("{}-absent", ids[1]);
    let before = counts(&r, &[ids[0], &unknown, ids[1]]);
    let shown = checked_page(
        &r,
        &[
            "show",
            ids[0],
            &unknown,
            ids[1],
            "--view-id",
            view,
            "--budget-kind",
            "bytes",
            "--budget",
            "16000",
        ],
    );
    assert_eq!(shown["items"].as_array().unwrap().len(), 3);
    assert_eq!(
        shown["items"][0],
        json!({"id":ids[0],"availability":"forgotten"})
    );
    assert_eq!(
        shown["items"][1],
        json!({"id":unknown,"availability":"not_found_in_scope"})
    );
    assert_eq!(shown["items"][2]["id"], ids[1]);
    assert!(shown["continuation"].is_null());
    let after = counts(&r, &[ids[0], &unknown, ids[1]]);
    assert_eq!(after, vec![before[0], before[1], before[2] + 1]);
    r.error(
        &["show", ids[0], &unknown, ids[1], "--budget", "1"],
        "budget_below_minimum",
    );
    assert_eq!(counts(&r, &[ids[0], &unknown, ids[1]]), after);

    // A final page with two placeholders can be smaller than a one-item page
    // carrying a cursor. It must still fit at its exact raw-byte boundary.
    let mut limit = 16000;
    let mut exact = false;
    for _ in 0..8 {
        let page = checked_page(
            &r,
            &[
                "show",
                ids[0],
                &unknown,
                "--view-id",
                view,
                "--budget-kind",
                "bytes",
                "--budget",
                &limit.to_string(),
            ],
        );
        assert_eq!(page["items"].as_array().unwrap().len(), 2);
        assert!(page["continuation"].is_null());
        let used = page["budget"]["used"].as_u64().unwrap() as usize;
        if used == limit {
            exact = true;
            break;
        }
        limit = used;
    }
    assert!(exact, "fixture must reach an exact byte bound");
}
