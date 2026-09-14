mod support;

use serde_json::{Value, json};
use support::{Repo, git};

fn reported_note(r: &Repo, text: &str) -> Value {
    r.write("checked.rs", "synthetic checked content");
    let report = json!({
        "command":"synthetic check","revision":r.git(&["rev-parse","HEAD"]),
        "object_format":"sha1","environment":"synthetic environment",
        "result":"reported pass","reported_by":"fixture","evidence":["checked.rs"],
        "evidence_content":[{"path":"checked.rs",
            "content_sha256":memq::util::hash(b"synthetic checked content")}]
    });
    r.write("report.json", serde_json::to_vec(&report).unwrap());
    r.ok(&[
        "note",
        "--kind",
        "verification",
        "--text",
        text,
        "--idempotency-key",
        "reported-check",
        "--verification",
        r.root.join("report.json").to_str().unwrap(),
    ])
}

fn read(r: &Repo, args: &[&str]) -> Value {
    let raw = r
        .command()
        .args(args)
        .args(["--budget-kind", "bytes", "--budget", "40000"])
        .output()
        .unwrap();
    assert!(raw.status.success(), "{:?}", raw);
    let mut value: Value = serde_json::from_slice(&raw.stdout).unwrap();
    assert_eq!(value["budget"]["used"], raw.stdout.len());
    assert!(raw.stdout.len() <= 40000);
    value["items"] = json!(support::expanded_items(&value));
    value
}

#[test]
fn retained_verification_rechecks_working_files_and_keeps_original_evidence() {
    let r = Repo::initialized();
    let note = reported_note(&r, "A reported verification");
    let id = note["id"].as_str().unwrap();
    let note_path = r.root.join(note["path"].as_str().unwrap());
    let note_bytes = std::fs::read(&note_path).unwrap();
    let initial = read(&r, &["brief"]);
    let view = initial["freshness"]["view_id"].as_str().unwrap();
    let original = &initial["items"][0];
    assert_eq!(original["applicability"], "current");
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let member_before: String = db
        .query_row(
            "SELECT member_json FROM view_items WHERE view_id=?1 AND item_id=?2",
            [view, id],
            |row| row.get(0),
        )
        .unwrap();
    drop(db);

    for expected in ["stale", "unknown", "current"] {
        match expected {
            "stale" => r.write("checked.rs", "changed after the view was saved"),
            "unknown" => std::fs::remove_file(r.root.join("checked.rs")).unwrap(),
            _ => r.write("checked.rs", "synthetic checked content"),
        }
        for compact in [false, true] {
            for mut args in [
                vec!["show", id, "--view-id", view],
                vec![
                    "search",
                    "reported",
                    "--incoming",
                    "true",
                    "--view-id",
                    view,
                ],
                vec!["brief", "--view-id", view],
            ] {
                if compact {
                    args.push("--compact");
                }
                let response = read(&r, &args);
                assert_eq!(response["freshness"]["view_id"], view);
                assert_eq!(response["items"].as_array().unwrap().len(), 1);
                let item = &response["items"][0];
                assert_eq!(item["applicability"], expected, "{args:?}");
                assert_eq!(item["verification"]["applicability"], expected);
                assert_eq!(item["claim"], "reported");
                assert_eq!(
                    item["verification"]["environment_applicability"],
                    "not_checked"
                );
                for field in ["id", "text", "pointer", "acceptance", "attribution"] {
                    assert_eq!(item[field], original[field]);
                }
                for field in [
                    "command",
                    "revision",
                    "object_format",
                    "environment",
                    "result",
                    "evidence",
                    "evidence_content",
                    "reported_by",
                ] {
                    assert_eq!(item["verification"][field], original["verification"][field]);
                }
                if !compact {
                    assert_eq!(item["version_id"], original["version_id"]);
                    assert_eq!(item["observation"], original["observation"]);
                }
            }
        }
    }
    assert_eq!(std::fs::read(note_path).unwrap(), note_bytes);
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    let member_after: String = db
        .query_row(
            "SELECT member_json FROM view_items WHERE view_id=?1 AND item_id=?2",
            [view, id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(member_after, member_before);
}

#[test]
fn retained_verification_uses_each_branch_and_incoming_members_saved_revision() {
    let r = Repo::initialized();
    let note = reported_note(&r, "A reported verification");
    let id = note["id"].as_str().unwrap();
    r.commit("synthetic reported evidence");
    let side = r.linked("side");
    std::fs::write(side.join("unrelated.txt"), "unrelated incoming change").unwrap();
    git(&side, &["add", "unrelated.txt"]);
    git(
        &side,
        &[
            "commit",
            "-q",
            "--no-gpg-sign",
            "-m",
            "synthetic incoming change",
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

    let live = read(&r, &["brief"]);
    let branches = read(&r, &["brief", "--branches", "main"]);
    for response in [&live, &branches] {
        assert_eq!(response["items"].as_array().unwrap().len(), 2);
        assert!(
            response["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["applicability"] == "current")
        );
    }
    r.write("checked.rs", "changed only in the current checkout");
    for (original, scoped) in [(&live, false), (&branches, true)] {
        for compact in [false, true] {
            let mut args = vec![
                "show",
                id,
                "--incoming",
                "true",
                "--view-id",
                original["freshness"]["view_id"].as_str().unwrap(),
            ];
            if scoped {
                args.extend(["--branches", "main"]);
            }
            if compact {
                args.push("--compact");
            }
            let response = read(&r, &args);
            assert_eq!(response["items"].as_array().unwrap().len(), 2);
            for item in response["items"].as_array().unwrap() {
                let origin = &item["observation"]["origin"];
                assert_eq!(
                    item["applicability"],
                    if scoped || origin == "incoming" {
                        "current"
                    } else {
                        "stale"
                    }
                );
                let before = original["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|before| before["observation"]["origin"] == *origin)
                    .unwrap();
                assert_eq!(item["pointer"], before["pointer"]);
                assert_eq!(
                    item["verification"]["revision"],
                    before["verification"]["revision"]
                );
                assert_eq!(
                    item["observation"]["commit"],
                    before["observation"]["commit"]
                );
            }
        }
    }
}

#[test]
fn verification_continuations_recheck_files_after_a_new_view_without_mixing_evidence() {
    let r = Repo::initialized();
    let text = "دليل محفوظ. Synthetic reported evidence.\n".repeat(400);
    let note = reported_note(&r, &text);
    let id = note["id"].as_str().unwrap();
    for compact in [false, true] {
        r.write("checked.rs", "synthetic checked content");
        let view = read(&r, &["brief"]);
        let view_id = view["freshness"]["view_id"].as_str().unwrap();
        let mut args = vec![
            "show",
            id,
            "--view-id",
            view_id,
            "--budget-kind",
            "bytes",
            "--budget",
            "6000",
        ];
        if compact {
            args.push("--compact");
        }
        let raw = r.run(&args);
        assert!(raw.status.success(), "{raw:?}");
        let mut page: Value = serde_json::from_slice(&raw.stdout).unwrap();
        assert!(page["continuation"].is_string());
        assert_eq!(page["items"][0]["applicability"], "current");
        let mut restored = page["items"][0]["text"].as_str().unwrap().to_owned();
        r.write("checked.rs", "changed during pagination");
        let current = read(&r, &["brief"]);
        assert_ne!(current["freshness"]["view_id"], view_id);
        let mut pages = 1;
        while let Some(cursor) = page["continuation"].as_str() {
            let mut next = args.clone();
            next.extend(["--continuation", cursor]);
            let raw = r.run(&next);
            assert!(raw.status.success(), "{raw:?}");
            page = serde_json::from_slice(&raw.stdout).unwrap();
            assert_eq!(page["budget"]["used"], raw.stdout.len());
            assert!(raw.stdout.len() <= 6000);
            assert_eq!(page["freshness"]["view_id"], view_id);
            assert_eq!(page["freshness"]["status"], "stale");
            assert_eq!(page["items"][0]["applicability"], "stale");
            assert_eq!(page["items"][0]["text_range"][0], restored.len());
            assert_eq!(page["items"][0]["pointer"], view["items"][0]["pointer"]);
            assert_eq!(
                page["items"][0]["verification"]["evidence_content"],
                view["items"][0]["verification"]["evidence_content"]
            );
            restored.push_str(page["items"][0]["text"].as_str().unwrap());
            pages += 1;
            assert!(pages < 30);
        }
        assert_eq!(restored, text);
    }
}
