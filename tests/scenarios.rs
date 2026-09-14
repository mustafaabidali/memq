mod support;
use serde_json::{Value, json};
use std::fs;
use std::process::Command;
use support::{Repo, git};

fn project(records: Value) -> Repo {
    let r = Repo::initialized();
    r.record_source();
    r.records(records);
    r.commit("synthetic project records");
    r
}
fn first_id(v: &Value) -> &str {
    v["items"][0]["id"].as_str().unwrap()
}
fn brief_at(r: &Repo, path: &std::path::Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_memq"))
        .arg("--repo")
        .arg(path)
        .args(args)
        .env("MEMQ_DATA_DIR", &r.data)
        .env("MEMQ_NOW", "2026-09-13T20:00:00Z")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}
fn remote_project() -> (Repo, std::path::PathBuf) {
    let r = project(
        json!([{"id":"D-provider","status":"proposed","decider":"owner","reason":"Provider approval pending"}]),
    );
    let remote = r.bare_remote();
    r.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    r.add_source("[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=2\nmin_interval_seconds=0\n");
    r.commit("remote scope");
    r.git(&["push", "-q", "-u", "origin", "main"]);
    git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let clone = r.clone_repo();
    git(
        &clone,
        &["remote", "set-url", "origin", remote.to_str().unwrap()],
    );
    (r, clone)
}

#[test]
fn scenario_01_budget_holds_omitted_searchable() {
    let r = project(json!(
        (0..80)
            .map(
                |n| json!({"id":format!("work-{n}"),"text":"Historical work evidence ".repeat(10)})
            )
            .collect::<Vec<_>>()
    ));
    let out = r.run(&["brief", "--budget", "1800"]);
    assert!(out.status.success());
    support::budget_check(&out.stdout);
    let b: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(b["omitted_count"].as_u64().unwrap() > 0);
    assert!(
        !r.ok(&["search", "work-79"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scenario_02_task_switch_keeps_records() {
    let r = project(
        json!([{"id":"a","task":"a","text":"First task"},{"id":"b","task":"b","text":"Second task"}]),
    );
    let a = r.ok(&["brief", "--task", "a"]);
    let b = r.ok(&["brief", "--task", "b"]);
    assert_ne!(a["freshness"]["view_id"], b["freshness"]["view_id"]);
    assert!(first_id(&a).ends_with(":a"));
    assert!(first_id(&b).ends_with(":b"));
    assert_eq!(
        r.ok(&["search", "First task"])["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn scenario_03_local_acceptance_replaces_pending() {
    let r = project(
        json!([{"id":"D-choice","status":"proposed","decider":"owner","reason":"Pending review"}]),
    );
    let before = r.ok(&["brief"]);
    r.records(json!([{"id":"D-choice","status":"accepted","decider":"maintainer","reason":"Reviewed compatibility"}]));
    r.commit("authorized acceptance");
    let b = r.ok(&["brief"]);
    assert_eq!(b["items"][0]["acceptance"], "accepted");
    assert_eq!(b["items"][0]["reason"], "Reviewed compatibility");
    assert_eq!(b["items"][0]["attribution"], "maintainer");
    assert_eq!(first_id(&b), first_id(&before));
    assert!(
        b["items"][0]["conflict_with"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scenario_04_incoming_resolution_separate() {
    let (r, clone) = remote_project();
    r.ok(&["brief"]);
    fs::write(clone.join("records.json"),serde_json::to_vec(&json!({"records":[{"id":"D-provider","status":"accepted","decider":"maintainer","reason":"Recorded provider readiness"}]})).unwrap()).unwrap();
    git(&clone, &["add", "records.json"]);
    git(
        &clone,
        &["commit", "-q", "--no-gpg-sign", "-m", "upstream acceptance"],
    );
    git(&clone, &["push", "-q", "origin", "main"]);
    let head = r.git(&["rev-parse", "HEAD"]);
    let index = fs::read(r.root.join(".git/index")).unwrap();
    fs::write(r.root.join(".git/FETCH_HEAD"), "sentinel\n").unwrap();
    let b = r.ok(&["brief", "--budget", "10000"]);
    assert_eq!(b["freshness"]["remote"]["incoming_commits"], 1);
    let items = b["items"].as_array().unwrap();
    assert!(items.iter().any(|v| v["observation"]["origin"] == "local"
        && v["native_status"] == "proposed"
        && v["section"] == "resolved_upstream_not_local"));
    assert!(
        items
            .iter()
            .any(|v| v["observation"]["origin"] == "incoming" && v["acceptance"] == "accepted")
    );
    assert_eq!(r.git(&["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(r.root.join(".git/index")).unwrap(), index);
    assert_eq!(
        fs::read_to_string(r.root.join(".git/FETCH_HEAD")).unwrap(),
        "sentinel\n"
    );
}

#[test]
fn scenario_05_changed_code_missing_rationale() {
    let r = project(json!([{"id":"history","text":"Previous task"}]));
    r.ok(&["brief"]);
    r.write("src/callback.rs", "fn changed() {}\n");
    r.commit("code without a decision");
    let b = r.ok(&["brief"]);
    assert_eq!(
        b["changes_since"]["rationale"],
        "not_found_in_observed_sources"
    );
    assert!(
        b["changes_since"]["paths"]
            .as_array()
            .unwrap()
            .contains(&json!("src/callback.rs"))
    );
}

#[test]
fn scenario_06_branch_scope_and_conflict_view() {
    let r = project(
        json!([{"id":"D-mode","status":"accepted","decider":"owner","reason":"Use first mode"}]),
    );
    let side = r.linked("side");
    fs::write(side.join("records.json"),serde_json::to_vec(&json!({"records":[{"id":"D-mode","status":"accepted","decider":"owner","reason":"Use second mode"}]})).unwrap()).unwrap();
    git(&side, &["add", "records.json"]);
    git(
        &side,
        &["commit", "-q", "--no-gpg-sign", "-m", "branch decision"],
    );
    let main = r.ok(&["brief"]);
    assert_eq!(main["items"][0]["reason"], "Use first mode");
    let combined = r.ok(&["brief", "--branches", "main,side", "--budget", "10000"]);
    let items = combined["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .all(|v| v["flags"].as_array().unwrap().contains(&json!("conflict")))
    );
    let changed = r.ok(&["brief", "--branches", "side", "--budget", "10000"]);
    assert_eq!(changed["items"].as_array().unwrap().len(), 1);
    assert_ne!(
        combined["freshness"]["view_id"],
        changed["freshness"]["view_id"]
    );
}

#[test]
fn scenario_07_dirty_decision_not_shared() {
    let r = project(json!([{"id":"D-dirty","status":"proposed","decider":"owner"}]));
    r.ok(&["brief"]);
    r.records(
        json!([{"id":"D-dirty","status":"accepted","decider":"owner","reason":"Local review"}]),
    );
    let b = r.ok(&["brief"]);
    assert_eq!(b["items"][0]["committed"], false);
    assert_eq!(b["items"][0]["observation"]["dirty"], true);
    assert_eq!(b["items"][0]["acceptance"], "accepted");
}

#[test]
fn scenario_08_rename_remove_revert_pointers() {
    let r = Repo::initialized();
    r.write("design.md", "# Design\nOriginal reasoning\n");
    r.add_source("[[source]]\nid='design'\nkind='markdown'\npath='design.md'\n");
    r.commit("design source");
    let old = r.ok(&["brief"]);
    let id = first_id(&old);
    r.git(&["mv", "design.md", "renamed.md"]);
    let config = fs::read_to_string(r.root.join(".memq/config.toml"))
        .unwrap()
        .replace("path='design.md'", "path='renamed.md'");
    r.write(".memq/config.toml", config);
    let renamed = r.ok(&["brief"]);
    assert_ne!(first_id(&renamed), id);
    let historical = r.ok(&["show", id]);
    assert_eq!(
        historical["items"][0]["pointer"],
        old["items"][0]["pointer"]
    );
    assert_eq!(historical["items"][0]["availability"], "source_missing");
    r.git(&["mv", "renamed.md", "design.md"]);
    let config = fs::read_to_string(r.root.join(".memq/config.toml"))
        .unwrap()
        .replace("path='renamed.md'", "path='design.md'");
    r.write(".memq/config.toml", config);
    assert_eq!(
        r.ok(&["brief"])["items"][0]["version_id"],
        old["items"][0]["version_id"]
    );
}

#[test]
fn scenario_09_ancestry_not_timestamps() {
    let r = project(json!([{"id":"state","text":"one"}]));
    let old = r.ok(&["brief"]);
    r.records(json!([{"id":"state","text":"two"}]));
    r.git(&["add", "records.json"]);
    r.git(&[
        "commit",
        "--amend",
        "-q",
        "--no-gpg-sign",
        "-m",
        "rewritten history",
    ]);
    let current = r.ok(&["brief"]);
    assert_ne!(current["scope"]["head"], old["scope"]["head"]);
    assert_eq!(
        current["changes_since"]["ancestry"],
        "rewritten_or_divergent"
    );
}

#[test]
fn scenario_10_worktree_isolation() {
    let r = project(json!([{"id":"same","text":"mainexclusive"}]));
    let side = r.linked("side");
    fs::write(
        side.join("records.json"),
        serde_json::to_vec(&json!({"records":[{"id":"same","text":"sideexclusive"}]})).unwrap(),
    )
    .unwrap();
    r.ok(&["brief"]);
    brief_at(&r, &side, &["brief"]);
    assert!(
        r.ok(&["search", "sideexclusive"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        brief_at(&r, &side, &["search", "mainexclusive"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        r.ok(&["search", "mainexclusive"])["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mainexclusive")
    );
}

#[test]
fn scenario_11_missed_notification_reconciles() {
    let r = project(json!([{"id":"state","text":"before"}]));
    let old = r.ok(&["brief"]);
    let path = r.root.join("records.json");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    r.records(json!([{"id":"state","text":"after!"}]));
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_ne!(
        r.ok(&["brief"])["freshness"]["view_id"],
        old["freshness"]["view_id"]
    );
}

#[test]
fn scenario_12_kill_before_publish() {
    let r = project(json!([{"id":"state","text":"before"}]));
    let old = r.ok(&["brief"]);
    r.records(json!([{"id":"state","text":"after"}]));
    let out = r
        .command()
        .env("MEMQ_FAULT", "before_publish")
        .arg("brief")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(86));
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM views WHERE view_id=?1 AND is_current=1",
            [old["freshness"]["view_id"].as_str().unwrap()],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(db);
    assert_ne!(
        r.ok(&["brief"])["freshness"]["view_id"],
        old["freshness"]["view_id"]
    );
}

#[test]
fn scenario_13_rebuild_from_sources() {
    let r = project(json!([{"id":"state","text":"survives"}]));
    let old = r.ok(&["brief"]);
    fs::remove_file(r.db_path()).unwrap();
    assert_eq!(
        r.ok(&["brief"])["items"][0]["version_id"],
        old["items"][0]["version_id"]
    );
}

#[test]
fn scenario_14_report_gap_user_restores() {
    let r = project(json!([{"id":"state","text":"gone"}]));
    r.ok(&["brief"]);
    fs::remove_file(r.root.join("records.json")).unwrap();
    fs::remove_file(r.db_path()).unwrap();
    r.error(&["brief"], "recovery_limit_reached");
    r.records(json!([{"id":"state","text":"restored by user"}]));
    assert!(
        r.ok(&["brief"])["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("restored by user")
    );
}

#[test]
fn scenario_15_tombstone_survives_replay() {
    let r = project(json!([{"id":"state","text":"forget me"}]));
    let old = r.ok(&["brief"]);
    r.ok(&["forget", first_id(&old)]);
    assert!(r.root.join("records.json").exists());
    r.ok(&["rebuild"]);
    assert!(
        r.ok(&["search", "forget me"])["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn scenario_16_offline_unknown_remote() {
    let (r, _clone) = remote_project();
    let observed = r.ok(&["brief"]);
    fs::rename(
        r.temp.path().join("remote.git"),
        r.temp.path().join("offline.git"),
    )
    .unwrap();
    let start = std::time::Instant::now();
    let b = r.ok(&["brief"]);
    assert_eq!(b["freshness"]["remote"]["status"], "failed");
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(b["scope"]["head"], observed["scope"]["head"]);
    assert_eq!(
        b["freshness"]["remote"]["observed_at"],
        observed["freshness"]["remote"]["observed_at"]
    );
}

#[test]
fn scenario_17_code_tool_failure_isolated() {
    let r =
        project(json!([{"id":"blocker","status":"blocked","text":"Continuity without a graph"}]));
    let b = r.ok(&["brief"]);
    assert_eq!(b["coverage"]["code"]["status"], "unavailable");
    assert_eq!(b["freshness"]["status"], "current");
    assert_eq!(b["items"][0]["section"], "blockers");
}

#[test]
fn scenario_18_outside_git_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_memq"))
        .arg("--repo")
        .arg(temp.path())
        .arg("brief")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap()["error"]["code"],
        "not_a_git_repository"
    );
}
