mod support;

#[test]
fn trailing_space_in_repository_path_is_not_trimmed_into_another_repository() {
    let r = support::Repo::new();
    let other = r.temp.path().join("project ");
    std::fs::create_dir(&other).unwrap();
    support::git(&other, &["init", "-q", "-b", "main"]);
    let a = memq::repository::Repository::discover(&r.root).unwrap();
    let b = memq::repository::Repository::discover(&other).unwrap();
    assert_ne!(a.clone_id, b.clone_id);
    assert_eq!(b.root, std::fs::canonicalize(other).unwrap());
    assert_eq!(
        b.text(&["rev-parse", "--show-toplevel"]).unwrap(),
        b.root.to_string_lossy()
    );
}

#[cfg(unix)]
#[test]
fn old_git_worktree_listing_preserves_note_retries_and_unusual_paths() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let r = support::Repo::initialized();
    r.commit("configured fixture");
    let linked = r.temp.path().join("linked \"\\عربي\nworktree ");
    r.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "side",
        linked.to_str().unwrap(),
    ]);
    let primary = r.ok(&[
        "note",
        "--text",
        "Primary inspection",
        "--idempotency-key",
        "primary-retry",
    ]);
    let command = |root: &std::path::Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_memq"));
        c.arg("--repo")
            .arg(root)
            .env("MEMQ_DATA_DIR", &r.data)
            .env("MEMQ_NOW", "2026-09-13T20:00:00Z");
        c
    };
    let linked_note_args = [
        "note",
        "--text",
        "Linked inspection",
        "--idempotency-key",
        "linked-retry",
    ];
    let first = command(&linked).args(linked_note_args).output().unwrap();
    assert!(first.status.success());
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    std::fs::remove_dir_all(&r.data).unwrap();

    let inherited = std::env::var_os("PATH").unwrap();
    let real_git = std::env::split_paths(&inherited)
        .map(|p| p.join("git"))
        .find(|p| p.is_file())
        .unwrap();
    let bin = r.temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let wrapper = bin.join("git");
    std::fs::write(
        &wrapper,
        r#"#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
if args[-4:] == ["worktree", "list", "--porcelain", "-z"]:
    with open(os.environ["MEMQ_TEST_GIT_PROBE"], "a") as f:
        f.write("unsupported-z\n")
    print("error: unknown switch `z'", file=sys.stderr)
    sys.exit(129)
os.execv(os.environ["MEMQ_TEST_REAL_GIT"], ["git", *args])
"#,
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&inherited)))
        .unwrap();
    let probe = r.temp.path().join("git-probe.txt");
    let old_git_command = |root: &std::path::Path| {
        let mut c = command(root);
        c.env("PATH", &path)
            .env("MEMQ_TEST_REAL_GIT", &real_git)
            .env("MEMQ_TEST_GIT_PROBE", &probe);
        c
    };
    let brief = old_git_command(&linked).arg("brief").output().unwrap();
    assert!(
        brief.status.success(),
        "{}",
        String::from_utf8_lossy(&brief.stdout)
    );
    let retry = old_git_command(&linked)
        .args(linked_note_args)
        .output()
        .unwrap();
    assert!(
        retry.status.success(),
        "{}",
        String::from_utf8_lossy(&retry.stdout)
    );
    assert!(probe.exists(), "the unsupported option was never exercised");
    let retry: serde_json::Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(retry["id"], first["id"]);
    let retry = old_git_command(&r.root)
        .args([
            "note",
            "--text",
            "Primary inspection",
            "--idempotency-key",
            "primary-retry",
        ])
        .output()
        .unwrap();
    assert!(retry.status.success());
    let retry: serde_json::Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(retry["id"], primary["id"]);
    for root in [&r.root, &linked] {
        assert_eq!(
            std::fs::read_dir(root.join(".memq/notes")).unwrap().count(),
            1
        );
    }
    let common = memq::repository::Repository::discover(&r.root)
        .unwrap()
        .common;
    let entry = std::fs::read_dir(common.join("worktrees"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    std::fs::write(entry.path().join("gitdir"), "ambiguous\nbacklink\n").unwrap();
    let rejected = old_git_command(&r.root)
        .args([
            "note",
            "--text",
            "Must not save",
            "--idempotency-key",
            "bad-backlink",
        ])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    let rejected: serde_json::Value = serde_json::from_slice(&rejected.stdout).unwrap();
    assert_eq!(rejected["error"]["code"], "git_error");
    for root in [&r.root, &linked] {
        assert_eq!(
            std::fs::read_dir(root.join(".memq/notes")).unwrap().count(),
            1
        );
    }
}
use memq::config::Config;
use memq::util;
use support::Repo;

#[test]
fn outside_git_and_unknown_config_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(memq::repository::Repository::discover(tmp.path()).is_err());
    let r = Repo::initialized();
    let original = std::fs::read_to_string(r.root.join(".memq/config.toml")).unwrap();
    for bad in [
        original.replace("format = 1", "format = 2"),
        original.replace("format = 1", "surprise = true\nformat = 1"),
        original.replace("memq-notes", "code-index"),
        original.replace("id = \"notes\"", "id = \"harness-omp\""),
        original.replace("id = \"notes\"", "id = \"UPPER\""),
        original.replace("path = \".memq/notes\"", "path = \"../outside\""),
    ] {
        assert!(Config::parse(&bad).is_err(), "{bad}");
    }
    assert!(
        Config::parse(
            &(original.clone() + "\n[[source]]\nid=\"notes\"\nkind=\"markdown\"\npath=\"x.md\"\n")
        )
        .is_err()
    );
}

#[test]
fn invalid_config_diagnostics_do_not_echo_secret_values() {
    let r = Repo::initialized();
    let path = r.root.join(".memq/config.toml");
    let secret = "password=made-up-fixture-value";
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        original.replace("budget = 4000", &format!("budget = \"{secret}\"")),
    )
    .unwrap();
    let output = r.run(&["brief"]);
    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.contains(secret),
        "invalid config echoed a secret-bearing value"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    assert!(stdout.contains("[REDACTED]"));
}

#[test]
fn canonical_identity_round_trips() {
    for raw in [
        "one:two",
        "%26a",
        "عَرَبِيّ/إجراء",
        "path/file name.md",
        "x\n\"y",
    ] {
        assert_eq!(util::unescape(&util::escape(raw)).unwrap(), raw);
    }
    assert!(util::unescape("%3a").is_err());
    assert!(util::unescape("%41").is_err());
    assert_eq!(
        util::hash_json(&serde_json::json!({"b":1.0,"a":"x"})).unwrap(),
        util::hash_json(&serde_json::json!({"a":"x","b":1})).unwrap()
    );
}

#[test]
fn source_removal_requires_explicit_reconciliation() {
    let r = Repo::initialized();
    r.record_source();
    r.records(serde_json::json!([{"id":"old","status":"pending"}]));
    r.ok(&["brief"]);
    let path = r.root.join(".memq/config.toml");
    std::fs::write(
        &path,
        std::fs::read_to_string(&path)
            .unwrap()
            .replace("id = \"records\"", "id = \"renamed\""),
    )
    .unwrap();
    r.error(&["brief"], "source_identity_missing");
    std::fs::remove_dir_all(r.store_root().join("generations")).unwrap();
    r.error(&["brief"], "source_identity_missing");
    r.ok(&["reconcile", "--allow-source-removal"]);
    r.ok(&["brief"]);
}
