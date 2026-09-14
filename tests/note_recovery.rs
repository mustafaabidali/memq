mod support;
use support::Repo;

const OPAQUE_KEY_A: &str = "inspection-A7b9C2d4E6f8G0h1J3k5L7m9N2p4Q6r8";
const OPAQUE_KEY_B: &str = "inspection-B8c0D3e5F7g9H1j2K4m6N8p0Q3r5S7t9";

#[test]
fn redaction_never_merges_distinct_note_operation_keys() {
    let r = Repo::initialized();
    assert_eq!(
        memq::redact::text(OPAQUE_KEY_A),
        memq::redact::text(OPAQUE_KEY_B)
    );
    let a = r.ok(&[
        "note",
        "--text",
        "First inspection",
        "--idempotency-key",
        OPAQUE_KEY_A,
    ]);
    let b = r.ok(&[
        "note",
        "--text",
        "Second inspection",
        "--idempotency-key",
        OPAQUE_KEY_B,
    ]);
    assert_ne!(a["id"], b["id"]);
    for (result, key) in [(&a, OPAQUE_KEY_A), (&b, OPAQUE_KEY_B)] {
        let bytes = std::fs::read(r.root.join(result["path"].as_str().unwrap())).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(key));
        let note: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            note["provenance"]["idempotency_key_sha256"],
            memq::util::hash(key)
        );
    }
    std::fs::remove_file(r.db_path()).unwrap();
    assert_eq!(
        a["id"],
        r.ok(&[
            "note",
            "--text",
            "First inspection",
            "--idempotency-key",
            OPAQUE_KEY_A
        ])["id"]
    );
    assert_eq!(
        b["id"],
        r.ok(&[
            "note",
            "--text",
            "Second inspection",
            "--idempotency-key",
            OPAQUE_KEY_B
        ])["id"]
    );
    assert_eq!(
        std::fs::read_dir(r.root.join(".memq/notes"))
            .unwrap()
            .count(),
        2
    );
}

#[test]
fn lost_legacy_redacted_key_never_silently_creates_a_duplicate() {
    let r = Repo::initialized();
    let result = r.ok(&[
        "note",
        "--text",
        "Keep the legacy note",
        "--idempotency-key",
        OPAQUE_KEY_A,
    ]);
    let path = r.root.join(result["path"].as_str().unwrap());
    let mut note: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    note["provenance"]
        .as_object_mut()
        .unwrap()
        .remove("idempotency_key_sha256");
    std::fs::write(&path, serde_json::to_vec(&note).unwrap()).unwrap();
    std::fs::remove_file(r.db_path()).unwrap();
    r.error(
        &[
            "note",
            "--text",
            "Keep the legacy note",
            "--idempotency-key",
            OPAQUE_KEY_A,
        ],
        "legacy_idempotency_key_unrecoverable",
    );
    assert_eq!(
        std::fs::read_dir(r.root.join(".memq/notes"))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap()).unwrap(),
        note
    );
    assert!(
        r.ok(&["show", result["id"].as_str().unwrap()])["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Keep the legacy note")
    );
}

#[test]
fn note_retry_collision_and_unrelated_staging() {
    let r = Repo::initialized();
    r.write("unrelated.txt", "Keep staged\n");
    r.git(&["add", "unrelated.txt"]);
    let first = r.ok(&[
        "note",
        "--text",
        "Recorded progress",
        "--idempotency-key",
        "stable",
    ]);
    assert_eq!(
        first["id"],
        r.ok(&[
            "note",
            "--text",
            "Recorded progress",
            "--idempotency-key",
            "stable"
        ])["id"]
    );
    r.error(
        &[
            "note",
            "--text",
            "Different progress",
            "--idempotency-key",
            "stable",
        ],
        "idempotency_conflict",
    );
    assert!(
        r.git(&["diff", "--cached", "--name-only"])
            .contains("unrelated.txt")
    );
    assert_eq!(
        std::fs::read_dir(r.root.join(".memq/notes"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn every_interruption_recovers_one_note() {
    for fault in [
        "after_note_reservation",
        "after_note_tmp",
        "after_rename_before_stage",
    ] {
        let r = Repo::initialized();
        let out = r
            .command()
            .env("MEMQ_FAULT", fault)
            .args([
                "note",
                "--text",
                "Recover me",
                "--idempotency-key",
                OPAQUE_KEY_A,
            ])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(86));
        let db = rusqlite::Connection::open(r.db_path()).unwrap();
        let reserved: String = db
            .query_row("SELECT note_id FROM note_ops", [], |r| r.get(0))
            .unwrap();
        drop(db);
        let recovered = r.ok(&[
            "note",
            "--text",
            "Recover me",
            "--idempotency-key",
            OPAQUE_KEY_A,
        ]);
        assert_eq!(recovered["note_id"], reserved);
        assert_eq!(
            std::fs::read_dir(r.root.join(".memq/notes"))
                .unwrap()
                .count(),
            1
        );
    }
}

#[test]
fn live_writer_keeps_os_lock_regardless_of_clock() {
    let r = Repo::initialized();
    let ready = r.temp.path().join("paused");
    let mut child = r
        .command()
        .env("MEMQ_PAUSE_AT", "after_note_reservation")
        .env("MEMQ_PAUSE_FILE", &ready)
        .args(["note", "--text", "Serialized", "--idempotency-key", "same"])
        .spawn()
        .unwrap();
    for _ in 0..250 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(ready.exists(), "writer did not reach reservation");
    let second = r
        .command()
        .env("MEMQ_NOW", "2099-01-01T00:00:00Z")
        .args(["note", "--text", "Serialized", "--idempotency-key", "same"])
        .output()
        .unwrap();
    assert_eq!(second.status.code(), Some(4));
    child.kill().unwrap();
    child.wait().unwrap();
    r.ok(&["note", "--text", "Serialized", "--idempotency-key", "same"]);
    assert_eq!(
        std::fs::read_dir(r.root.join(".memq/notes"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn ignored_notes_and_git_lock_failures_still_report_durability() {
    let r = Repo::initialized();
    r.write(".gitignore", ".memq/notes/\n");
    let ignored = r.ok(&[
        "note",
        "--text",
        "Ignored evidence",
        "--idempotency-key",
        "ignored",
    ]);
    assert_eq!(ignored["durability"], "durable");
    assert_eq!(ignored["staging"], "ignored");
    r.write(".gitignore", "");
    r.write(".git/index.lock", "Synthetic held Git lock");
    let blocked = r.ok(&[
        "note",
        "--text",
        "Durable despite staging",
        "--idempotency-key",
        "staging",
    ]);
    assert_eq!(blocked["durability"], "durable");
    assert_eq!(blocked["staging"], "failed");
    std::fs::remove_file(r.root.join(".git/index.lock")).unwrap();
    let retry = r.ok(&[
        "note",
        "--text",
        "Durable despite staging",
        "--idempotency-key",
        "staging",
    ]);
    assert_eq!(retry["id"], blocked["id"]);
    assert_eq!(retry["staging"], "staged");
}

#[test]
fn linked_authored_notes_backfill_after_index_loss() {
    let r = Repo::initialized();
    r.commit("configuration");
    let side = r.linked("side");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_memq"))
        .env("MEMQ_DATA_DIR", &r.data)
        .arg("--repo")
        .arg(&side)
        .args([
            "note",
            "--text",
            "Side inspection",
            "--idempotency-key",
            "clone-key",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    std::fs::remove_dir_all(r.store_root().join("generations")).unwrap();
    // The key belongs to the side-branch scope; it cannot create a different
    // main-branch note after losing derived state.
    r.error(
        &[
            "note",
            "--text",
            "Main inspection",
            "--idempotency-key",
            "clone-key",
        ],
        "idempotency_conflict",
    );
    assert!(!r.root.join(".memq/notes").exists());
    assert_eq!(
        std::fs::read_dir(side.join(".memq/notes")).unwrap().count(),
        1
    );
}

#[test]
fn verification_checks_complete_file_evidence_and_retains_reported_claim() {
    use serde_json::json;
    let r = Repo::initialized();
    r.write("checked.rs", "synthetic code");
    r.write("unchecked.rs", "another file");
    let report = json!({
        "command":"cargo test", "revision":r.git(&["rev-parse","HEAD"]),
        "object_format":"sha1", "environment":"synthetic fixture", "result":"passed",
        "reported_by":"fixture",
        "evidence":["checked.rs"],
        "evidence_content":[{"path":"checked.rs","content_sha256":memq::util::hash(b"synthetic code")}]
    });
    r.write("verification.json", serde_json::to_vec(&report).unwrap());
    let note = r.ok(&[
        "note",
        "--kind",
        "verification",
        "--text",
        "Reported fixture check",
        "--idempotency-key",
        "verification",
        "--verification",
        r.root.join("verification.json").to_str().unwrap(),
    ]);
    let show = r.ok(&["show", note["id"].as_str().unwrap()]);
    assert_eq!(show["items"][0]["claim"], "reported");
    assert_eq!(show["items"][0]["applicability"], "current");
    assert_eq!(
        show["items"][0]["verification"]["environment_applicability"],
        "not_checked"
    );
    r.write("checked.rs", "changed code without a commit");
    assert_eq!(
        r.ok(&["show", note["id"].as_str().unwrap()])["items"][0]["applicability"],
        "stale"
    );
    let mut partial = report;
    partial["evidence"] = json!(["checked.rs", "unchecked.rs"]);
    r.write("verification.json", serde_json::to_vec(&partial).unwrap());
    let partial = r.ok(&[
        "note",
        "--kind",
        "verification",
        "--text",
        "Partial evidence",
        "--idempotency-key",
        "partial",
        "--verification",
        r.root.join("verification.json").to_str().unwrap(),
    ]);
    assert_eq!(
        r.ok(&["show", partial["id"].as_str().unwrap()])["items"][0]["applicability"],
        "unknown"
    );
}
