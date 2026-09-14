mod support;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use support::Repo;

const BUDGET: &str = "2000000";

fn command(r: &Repo) -> Command {
    let mut command = r.command();
    for name in ["MEMQ_OMP_STORE", "MEMQ_CODEX_STORE", "MEMQ_OPENCODE_STORE"] {
        command.env(name, "");
    }
    for name in [
        "MEMQ_CODE_REPORT",
        "MEMQ_FAULT",
        "MEMQ_PAUSE_AT",
        "MEMQ_PAUSE_FILE",
        "GIT_TRACE2_EVENT",
    ] {
        command.env_remove(name);
    }
    command
}

fn run(r: &Repo, args: &[&str]) -> Value {
    let out = command(r).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "command failed: {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn read(r: &Repo, args: &[&str], trace: Option<&Path>) -> Value {
    let mut command = command(r);
    command
        .args(args)
        .args(["--budget-kind", "bytes", "--budget", BUDGET]);
    if let Some(trace) = trace {
        command.env("GIT_TRACE2_EVENT", trace);
    }
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "read failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    support::budget_check(&out.stdout);
    serde_json::from_slice(&out.stdout).unwrap()
}

fn mcp_brief(r: &Repo, trace: Option<&Path>) -> Value {
    let mut command = command(r);
    if let Some(trace) = trace {
        command.env("GIT_TRACE2_EVENT", trace);
    }
    let mut child = command
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"brief","arguments":{
            "compact":true,"budget_kind":"bytes","budget":BUDGET.parse::<usize>().unwrap()
        }}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reply = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|reply| reply["id"] == 2)
        .unwrap();
    assert!(reply["error"].is_null(), "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let value = &reply["result"]["structuredContent"];
    support::budget_check(&serde_json::to_vec(value).unwrap());
    value.clone()
}

fn note_id(index: u128) -> String {
    ulid::Ulid::from(index).to_string()
}

fn note_path(index: u128) -> String {
    format!(".memq/notes/{}.json", note_id(index))
}

fn note(project: &str, index: u128, format: &str, timestamp: &str) -> Value {
    json!({
        "format":1,"id":note_id(index),"kind":"progress",
        "text":format!("Synthetic progress {index}"),"recorded_at":timestamp,
        "scope":{"project_id":project,"branch":null,"task":null},
        "provenance":{"harness":null,"session":null,"idempotency_key":format!("fixture-{index}")},
        "evidence":[],"verification":null,
        "observed":{"head":null,"object_format":format,"dirty":false}
    })
}

fn project_id(r: &Repo) -> String {
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(r.root.join(".memq/config.toml")).unwrap()).unwrap();
    config["project_id"].as_str().unwrap().to_owned()
}

fn fixture(count: usize, format: &str) -> Repo {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("project");
    let data = temp.path().join("data");
    fs::create_dir(&root).unwrap();
    let r = Repo { temp, root, data };
    r.git(&[
        "init",
        "-q",
        "-b",
        "main",
        &format!("--object-format={format}"),
    ]);
    r.write("seed.txt", "Synthetic project\n");
    r.commit("fixture seed");
    run(&r, &["init"]);
    let project = project_id(&r);
    for index in 1..=count as u128 {
        r.write(
            &note_path(index),
            serde_json::to_vec(&note(&project, index, format, "2026-09-13T20:00:00Z")).unwrap(),
        );
    }
    r.commit("synthetic notes");
    r
}

fn git_processes(trace: &Path) -> usize {
    let count = fs::read_to_string(trace)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["event"] == "start")
        .count();
    assert!(count > 0, "Git process tracing must be active");
    count
}

#[test]
fn hundreds_of_committed_notes_use_bounded_git_processes() {
    let mut counts = Vec::new();
    for count in [8, 500] {
        let r = fixture(count, "sha1");
        let revision = r.git(&["rev-parse", "HEAD"]);
        let tree = r.git(&["ls-tree", "-r", "HEAD", "--", ".memq/notes"]);
        let expected: BTreeMap<_, _> = tree
            .lines()
            .map(|line| {
                let (header, path) = line.split_once('\t').unwrap();
                (
                    path.to_owned(),
                    header.split_whitespace().nth(2).unwrap().to_owned(),
                )
            })
            .collect();
        let cold_trace = r.temp.path().join("cold-git.jsonl");
        let first = read(&r, &["brief"], Some(&cold_trace));
        let items = support::expanded_items(&first);
        assert_eq!(items.len(), count);
        for item in &items {
            let path = item["observation"]["source_path"].as_str().unwrap();
            assert_eq!(item["committed"], true);
            assert_eq!(item["observation"]["dirty"], false);
            assert_eq!(item["observation"]["commit"], revision);
            assert_eq!(item["observation"]["blob_oid"], expected[path]);
        }
        let warm_trace = r.temp.path().join("warm-git.jsonl");
        let warm = mcp_brief(&r, Some(&warm_trace));
        assert_eq!(warm["freshness"], first["freshness"]);
        let warm_items = support::expanded_items(&warm);
        assert_eq!(warm_items.len(), count);
        for (full, compact) in items.iter().zip(&warm_items) {
            assert_eq!(compact["id"], full["id"]);
            assert_eq!(compact["pointer"], full["pointer"]);
            assert_eq!(compact["committed"], true);
            assert_eq!(compact["observation"]["dirty"], false);
        }
        counts.push((git_processes(&cold_trace), git_processes(&warm_trace)));
    }
    // Count actual Git process starts, not Rust calls or elapsed time. A small
    // allowance permits fixed setup changes without allowing per-note spawns.
    assert!(
        counts[1].0 <= counts[0].0 + 8,
        "cold Git processes: {counts:?}"
    );
    assert!(
        counts[1].1 <= counts[0].1 + 8,
        "warm Git processes: {counts:?}"
    );
}

fn markdown_source(r: &Repo, id: &str, path: &str) {
    r.add_source(&format!(
        "[[source]]\nid={}\nkind=\"markdown\"\npath={}\n",
        json!(id),
        json!(path)
    ));
}

#[test]
fn batched_proof_preserves_dirty_ignored_and_untracked_notes_and_sources() {
    for format in ["sha1", "sha256"] {
        let r = fixture(3, format);
        let literal_path = if cfg!(unix) {
            ":(glob)guide[1]\tsection\nline.md"
        } else {
            "guide[1] section.md"
        };
        let long = format!(
            "# Guide\n{}\nTail\n",
            "Synthetic source evidence\n".repeat(4096)
        );
        r.write("assumed.md", &long);
        r.write(literal_path, "# Literal\nCommitted source evidence\n");
        r.write("ignored.md", "# Ignored\nWorking source evidence\n");
        r.write(".gitignore", format!("/ignored.md\n/{}\n", note_path(4)));
        markdown_source(&r, "assumed-guide", "assumed.md");
        markdown_source(&r, "literal-guide", literal_path);
        markdown_source(&r, "ignored-guide", "ignored.md");
        markdown_source(&r, "untracked-guide", "untracked.md");
        r.commit("configured synthetic sources");

        let project = project_id(&r);
        let mut changed = note(&project, 2, format, "2026-09-13T20:00:00Z");
        changed["text"] = json!("Ordinary working change");
        r.write(&note_path(2), serde_json::to_vec(&changed).unwrap());
        r.git(&[
            "update-index",
            "--assume-unchanged",
            &note_path(3),
            "assumed.md",
        ]);
        changed["id"] = json!(note_id(3));
        changed["provenance"]["idempotency_key"] = json!("fixture-3");
        r.write(&note_path(3), serde_json::to_vec(&changed).unwrap());
        let mut long = long.into_bytes();
        long[80_000] = b'Z';
        r.write("assumed.md", long);
        for index in [4, 5] {
            r.write(
                &note_path(index),
                serde_json::to_vec(&note(&project, index, format, "2026-09-13T20:00:00Z")).unwrap(),
            );
        }
        r.write("untracked.md", "# Untracked\nWorking source evidence\n");
        let full = read(&r, &["brief"], None);
        let full_items = support::expanded_items(&full);
        let expected: BTreeMap<_, _> = full_items
            .iter()
            .map(|item| {
                let path = item["observation"]["source_path"].as_str().unwrap();
                let committed = path == note_path(1) || path == literal_path;
                assert_eq!(item["committed"], committed, "source: {path}");
                assert_eq!(item["observation"]["dirty"], !committed, "source: {path}");
                assert_eq!(item["observation"]["blob_oid"].is_string(), committed);
                assert_eq!(item["observation"]["object_format"], format);
                assert!(item["pointer"].as_str().unwrap().starts_with(if committed {
                    "git:"
                } else {
                    "worktree:"
                }));
                (item["id"].as_str().unwrap().to_owned(), committed)
            })
            .collect();
        assert_eq!(expected.len(), 9);
        let compact = mcp_brief(&r, None);
        for item in support::expanded_items(&compact) {
            assert_eq!(item["committed"], expected[item["id"].as_str().unwrap()]);
            assert!(item["text"].is_string());
        }
    }
}

#[test]
fn revision_witness_requires_readable_blobs_and_keeps_stream_alignment() {
    let r = fixture(3, "sha1");
    let repo = memq::repository::Repository::discover(&r.root).unwrap();
    let revision = r.git(&["rev-parse", "HEAD"]);
    let first = fs::read(r.root.join(note_path(1))).unwrap();
    let second = fs::read(r.root.join(note_path(2))).unwrap();
    let third = fs::read(r.root.join(note_path(3))).unwrap();
    let oid = r.git(&["rev-parse", &format!("{revision}:{}", note_path(1))]);
    r.write(&note_path(1), b"Different current bytes");
    r.commit("different live bytes");
    let mut witness = repo.blob_witness(&revision, &[".memq/notes"]).unwrap();
    assert_eq!(
        witness.matching_oid(&note_path(1), &first).unwrap(),
        Some(oid.clone())
    );
    assert!(
        witness
            .matching_oid(&note_path(2), b"wrong size")
            .unwrap()
            .is_none()
    );
    let mut wrong = second.clone();
    wrong[0] = b'[';
    assert!(
        witness
            .matching_oid(&note_path(2), &wrong)
            .unwrap()
            .is_none()
    );
    assert!(
        witness
            .matching_oid(&note_path(3), &third)
            .unwrap()
            .is_some()
    );
    drop(witness);

    // Only a loose object created in this disposable fixture is removed.
    fs::remove_file(repo.common.join("objects").join(&oid[..2]).join(&oid[2..])).unwrap();
    let mut witness = repo.blob_witness(&revision, &[".memq/notes"]).unwrap();
    assert!(
        witness
            .matching_oid(&note_path(1), &first)
            .unwrap()
            .is_none()
    );
    assert!(
        witness
            .matching_oid(&note_path(2), &second)
            .unwrap()
            .is_some()
    );
    assert!(
        witness
            .matching_oid("absent.md", b"untracked")
            .unwrap()
            .is_none()
    );
}

fn assert_not_retained(r: &Repo, id: &str) {
    let db = rusqlite::Connection::open(r.db_path()).unwrap();
    for sql in [
        "SELECT count(*) FROM item_versions WHERE item_id=?1",
        "SELECT count(*) FROM view_items WHERE item_id=?1",
    ] {
        assert_eq!(
            db.query_row(sql, [id], |row| row.get::<_, i64>(0)).unwrap(),
            0
        );
    }
}

#[test]
fn timestamp_corrections_cannot_restore_known_forgotten_configured_ids() {
    for notes in [false, true] {
        let r = fixture(0, "sha1");
        let project = project_id(&r);
        let source = if notes { "notes" } else { "records" };
        if notes {
            let mut value = note(&project, 1, "sha1", "2026-09-13T20:00:00Z");
            value["text"] = json!("Synthetic forgotten body");
            r.write(&note_path(1), serde_json::to_vec(&value).unwrap());
        } else {
            r.add_source("[[source]]\nid=\"records\"\nkind=\"json-records\"\npath=\"records.json\"\ncollection=\"records\"\nid_field=\"id\"\n");
            r.records(json!([{"id":"known","text":"Synthetic forgotten body","recorded_at":"2026-09-13T20:00:00Z"}]));
        }
        let first = read(&r, &["brief"], None);
        let forgotten = support::expanded_items(&first)[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let purged = run(
            &r,
            &[
                "forget",
                "--source",
                source,
                "--after",
                "2026-09-13T19:00:00Z",
                "--before",
                "2026-09-13T20:30:00Z",
            ],
        );
        assert_eq!(purged["purge"], "complete");
        assert_not_retained(&r, &forgotten);
        if notes {
            for index in [1, 2] {
                let mut value = note(&project, index, "sha1", "2026-09-13T22:00:00Z");
                value["text"] = json!(if index == 1 {
                    "Synthetic forgotten body"
                } else {
                    "Distinct future evidence"
                });
                r.write(&note_path(index), serde_json::to_vec(&value).unwrap());
            }
        } else {
            r.records(json!([
                {"id":"known","text":"Synthetic forgotten body","recorded_at":"2026-09-13T22:00:00Z"},
                {"id":"new","text":"Distinct future evidence","recorded_at":"2026-09-13T22:00:00Z"}
            ]));
        }
        for rebuild in [false, true] {
            if rebuild {
                run(&r, &["rebuild"]);
            }
            for page in [read(&r, &["brief"], None), mcp_brief(&r, None)] {
                assert_eq!(page["freshness"]["status"], "current");
                let items = support::expanded_items(&page);
                assert_eq!(items.len(), 1);
                assert_ne!(items[0]["id"], forgotten);
                assert!(
                    items[0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("Distinct future evidence")
                );
                assert_not_retained(&r, &forgotten);
            }
        }
    }
}
