mod support;

use memq::capture::{self, CaptureCacheInput, Captured};
use memq::config::CaptureConfig;
use memq::repository::Repository;
use memq::tombstone::{Predicate, Tombstone};
use memq::util;
use serde_json::{Value, json};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use support::Repo;

const PROJECT: &str = "01J00000000000000000000000";

struct Journal {
    fixture: Repo,
    repo: Repository,
    config: CaptureConfig,
    kind: &'static str,
    path: PathBuf,
}

impl Journal {
    fn new(kind: &'static str) -> Self {
        let fixture = Repo::new();
        let repo = Repository::discover(&fixture.root).unwrap();
        let root = fixture.temp.path().join("sessions");
        fs::create_dir(&root).unwrap();
        let path = root.join("synthetic.jsonl");
        let mut config = CaptureConfig::default();
        match kind {
            "omp" => config.omp = Some(root.to_str().unwrap().into()),
            "codex" => config.codex = Some(root.to_str().unwrap().into()),
            _ => unreachable!(),
        }
        let journal = Self {
            fixture,
            repo,
            config,
            kind,
            path,
        };
        journal.replace(&[(1, "Original captured evidence")]);
        journal
    }

    fn header(&self) -> Value {
        match self.kind {
            "omp" => json!({"type":"session","version":3,"id":"cache-session",
                "cwd":self.fixture.root,"git":{"branch":"main"}}),
            "codex" => json!({"type":"session_meta","ordinal":0,
                "payload":{"id":"cache-session","cwd":self.fixture.root,
                    "git":{"branch":"main"}}}),
            _ => unreachable!(),
        }
    }

    fn record(&self, id: u64, text: &str) -> Value {
        match self.kind {
            "omp" => json!({"type":"message","id":id.to_string(),
                "timestamp":"2026-09-13T19:00:00Z","message":{"content":text}}),
            "codex" => json!({"type":"response_item","ordinal":id,
                "timestamp":"2026-09-13T19:00:00Z","payload":{"text":text}}),
            _ => unreachable!(),
        }
    }

    fn replace(&self, entries: &[(u64, &str)]) {
        let mut text = format!("{}\n", self.header());
        for (id, body) in entries {
            text.push_str(&format!("{}\n", self.record(*id, body)));
        }
        fs::write(&self.path, text).unwrap();
    }

    fn append(&self, id: u64, text: &str) {
        writeln!(
            fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap(),
            "{}",
            self.record(id, text)
        )
        .unwrap();
    }

    fn collect(&self, cache: &CaptureCacheInput, tombstones: &[Tombstone]) -> Captured {
        capture::collect(&self.repo, PROJECT, &self.config, tombstones, cache).unwrap()
    }

    fn cold(&self) -> Captured {
        self.collect(&CaptureCacheInput::default(), &[])
    }
}

fn semantics(captured: &Captured) -> Value {
    let items: Vec<_> = captured
        .items
        .iter()
        .map(|item| {
            let mut observation = serde_json::to_value(&item.observation).unwrap();
            observation
                .as_object_mut()
                .unwrap()
                .remove("observation_id");
            observation.as_object_mut().unwrap().remove("observed_at");
            json!({"id":item.id,"version_id":item.version_id,"payload":item.payload,
                "observation":observation})
        })
        .collect();
    json!({"items":items,"inventory":captured.inventory,"coverage":captured.coverage,
        "missing":captured.missing,"incomplete":captured.incomplete,"sources":captured.sources})
}

fn append_reuses_only_a_fully_verified_prefix(kind: &'static str) {
    let journal = Journal::new(kind);
    let first = journal.cold();
    let prefix_len = fs::metadata(&journal.path).unwrap().len();
    let cache = first.cache_input();
    let warm = journal.collect(&cache, &[]);
    assert_eq!(semantics(&warm), semantics(&first));
    assert_eq!(warm.work.parsed_values, 0);
    assert_eq!(warm.work.redacted_records, 0);
    assert_eq!(warm.work.reused_records, 1);
    assert_eq!(warm.work.verified_prefix_bytes, prefix_len);
    assert_eq!(warm.work.scope_discoveries, 1);
    journal.append(2, "New appended evidence");
    let appended = journal.collect(&cache, &[]);
    assert_eq!(semantics(&appended), semantics(&journal.cold()));
    assert_eq!(appended.items.len(), 2);
    assert_eq!(appended.work.parsed_values, 1);
    assert_eq!(appended.work.redacted_records, 1);
    assert_eq!(appended.work.reused_records, 1);
    assert_eq!(appended.work.verified_prefix_bytes, prefix_len);
    assert_eq!(appended.work.scope_discoveries, 1);
}

#[test]
fn omp_append_reuses_only_a_fully_verified_prefix() {
    append_reuses_only_a_fully_verified_prefix("omp");
}

#[test]
fn codex_append_reuses_only_a_fully_verified_prefix() {
    append_reuses_only_a_fully_verified_prefix("codex");
}

fn same_inode_middle_edits_invalidate_the_entire_prefix(kind: &'static str) {
    let journal = Journal::new(kind);
    let padding = "Synthetic padding for a long first record. ".repeat(160);
    journal.replace(&[
        (1, &padding),
        (2, "middle-record-one"),
        (3, "Later captured evidence"),
    ]);
    let first = journal.cold();
    let bytes = fs::read(&journal.path).unwrap();
    let before = fs::metadata(&journal.path).unwrap();
    let at = bytes
        .windows(b"middle-record-one".len())
        .position(|word| word == b"middle-record-one")
        .unwrap();
    assert!(at > 4096);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&journal.path)
        .unwrap();
    file.seek(SeekFrom::Start(at as u64)).unwrap();
    file.write_all(b"middle-record-two").unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    let changed = fs::metadata(&journal.path).unwrap();
    assert_eq!(changed.len(), before.len());
    assert_eq!(changed.modified().unwrap(), before.modified().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(changed.ino(), before.ino());
    }
    assert_eq!(&fs::read(&journal.path).unwrap()[..4096], &bytes[..4096]);
    let corrected = journal.collect(&first.cache_input(), &[]);
    assert_eq!(semantics(&corrected), semantics(&journal.cold()));
    assert_eq!(corrected.work.reused_records, 0);
    assert_eq!(corrected.work.verified_prefix_bytes, before.len());
    assert_ne!(corrected.items[1].version_id, first.items[1].version_id);
    assert_eq!(corrected.items[1].id, first.items[1].id);

    // An unreadable earlier record must invalidate reuse, even if all later
    // records and the file's inode, size, mtime, and first 4 KiB are unchanged.
    let line_start = bytes[..at].iter().rposition(|byte| *byte == b'\n').unwrap() + 1;
    file.seek(SeekFrom::Start(line_start as u64)).unwrap();
    file.write_all(b"!").unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    let corrupt = journal.collect(&corrected.cache_input(), &[]);
    assert_eq!(semantics(&corrupt), semantics(&journal.cold()));
    assert!(corrupt.incomplete);
    assert_eq!(corrupt.items.len(), 1);
    assert_eq!(corrupt.work.reused_records, 0);
    assert_eq!(corrupt.cursors[0].2["byte_offset"], line_start);
    assert_eq!(corrupt.cursors[0].2["complete"], false);
    assert!(corrupt.cache_input().cursors.is_empty());
}

#[test]
fn omp_same_inode_middle_edits_invalidate_the_entire_prefix() {
    same_inode_middle_edits_invalidate_the_entire_prefix("omp");
}

#[test]
fn codex_same_inode_middle_edits_invalidate_the_entire_prefix() {
    same_inode_middle_edits_invalidate_the_entire_prefix("codex");
}

#[test]
fn truncation_and_replacement_replay_without_old_members() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        journal.append(2, "Record removed by truncation");
        let first = journal.cold();
        journal.replace(&[(1, "Original captured evidence")]);
        let truncated = journal.collect(&first.cache_input(), &[]);
        assert_eq!(semantics(&truncated), semantics(&journal.cold()));
        assert_eq!(truncated.items.len(), 1);
        assert_eq!(truncated.items[0].version_id, first.items[0].version_id);
        assert_eq!(truncated.work.reused_records, 0);
        let replacement = journal.path.with_extension("replacement");
        fs::write(
            &replacement,
            format!(
                "{}\n{}\n",
                journal.header(),
                journal.record(1, "Replacement evidence")
            ),
        )
        .unwrap();
        fs::rename(replacement, &journal.path).unwrap();
        let replaced = journal.collect(&truncated.cache_input(), &[]);
        assert_eq!(semantics(&replaced), semantics(&journal.cold()));
        assert_eq!(replaced.work.reused_records, 0);
        assert_eq!(replaced.items[0].id, first.items[0].id);
        assert_ne!(replaced.items[0].version_id, first.items[0].version_id);
    }
}

#[test]
fn missing_or_corrupt_payloads_are_whole_file_cache_misses() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        journal.append(2, "Another retained version");
        let first = journal.cold();
        for mode in ["missing", "body", "native_id", "kind", "record"] {
            let mut cache = first.cache_input();
            // Leave the first body intact: a later failure must not leak a
            // partially reconstructed prefix into the fallback scan.
            let version = &first.items[1].version_id;
            if mode == "missing" {
                cache.payloads_by_version.remove(version);
            } else {
                let payload = cache.payloads_by_version.get_mut(version).unwrap();
                match mode {
                    "body" => payload.redacted_text = "Incorrect cached content".into(),
                    "native_id" => payload.native_id = "another-session/2".into(),
                    "kind" => payload.kind = "another-format".into(),
                    "record" => payload.record = json!({"unexpected":true}),
                    _ => unreachable!(),
                }
            }
            let result = journal.collect(&cache, &[]);
            assert_eq!(semantics(&result), semantics(&first), "{kind}: {mode}");
            assert_eq!(result.work.reused_records, 0, "{kind}: {mode}");
            assert_eq!(result.work.parsed_values, 3);
            assert_eq!(result.cache_input().referenced_versions().len(), 2);
        }
    }
}

#[test]
fn inconsistent_cursor_fields_and_bindings_miss_safely() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let first = journal.cold();
        let cases = [
            ("/complete", json!(false)),
            ("/byte_offset", json!(1)),
            ("/size", json!(1)),
            ("/inventory_sha256", json!("0".repeat(64))),
            ("/last_id_or_ordinal", json!("unseen-entry")),
            ("/fingerprint/inode", json!(0)),
            ("/fingerprint/device", json!(0)),
            ("/cache/format", json!(2)),
            ("/cache/prefix_sha256", json!("0".repeat(64))),
            ("/cache/prefix_len", json!(1)),
            ("/cache/header_end", json!(1)),
            ("/cache/binding/project_id", json!("another-project")),
            ("/cache/binding/clone_id", json!("another-clone")),
            ("/cache/binding/source_id", json!("another-source")),
            ("/cache/binding/source_path", json!("another.jsonl")),
            ("/cache/binding/source_root", json!("another-root")),
            ("/cache/binding/root_sha256", json!("0".repeat(64))),
            ("/cache/binding/parser_revision", json!("another-parser")),
            (
                "/cache/binding/redaction_revision",
                json!("another-redaction"),
            ),
            ("/cache/session/worktree", json!("another-worktree")),
            ("/cache/session/branch", json!("another-branch")),
            ("/cache/entries/0/byte_range/0", json!(0)),
            ("/cache/entries/0/source_sha256", json!("invalid")),
            ("/cache/entries/0/id", json!("")),
            ("/cache/entries/0/state/version_id", json!("0".repeat(64))),
        ];
        for (pointer, value) in cases {
            let mut cache = first.cache_input();
            let cursor = cache.cursors.values_mut().next().unwrap();
            *cursor.pointer_mut(pointer).unwrap() = value;
            // Preserve a valid JSON/checksum envelope so field consistency and
            // binding validation are exercised independently of corruption.
            cursor["cache_sha256"] = json!(util::hash_json(&cursor["cache"]).unwrap());
            let result = journal.collect(&cache, &[]);
            assert_eq!(semantics(&result), semantics(&first), "{kind}: {pointer}");
            assert_eq!(result.work.reused_records, 0, "{kind}: {pointer}");
            assert_eq!(result.work.parsed_values, 2, "{kind}: {pointer}");
        }
        let mut broken_checksum = first.cache_input();
        broken_checksum.cursors.values_mut().next().unwrap()["cache_sha256"] = json!("invalid");
        assert!(broken_checksum.referenced_versions().is_empty());
        assert_eq!(
            semantics(&journal.collect(&broken_checksum, &[])),
            semantics(&first)
        );
        let mut legacy = first.cache_input();
        legacy
            .cursors
            .values_mut()
            .next()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("cache");
        assert!(legacy.referenced_versions().is_empty());
        assert_eq!(semantics(&journal.collect(&legacy, &[])), semantics(&first));
    }
}

fn forget(item_id: String) -> Tombstone {
    let scope = Predicate {
        item_id: Some(item_id),
        project_id: None,
        source_id: None,
        after: None,
        before: None,
    };
    Tombstone {
        format: 1,
        identity_hash: scope.hash().unwrap(),
        scope,
        forgotten_at: "2026-09-13T20:00:00Z".into(),
        purge_epoch: util::id(),
    }
}

#[test]
fn current_tombstones_precede_payload_lookup_and_remove_candidate_refs() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let first = journal.cold();
        let forgotten = &first.items[0];
        let tombstones = [forget(forgotten.id.clone())];
        let mut cache = first.cache_input();
        // A purge has already removed this immutable version from storage.
        cache.payloads_by_version.remove(&forgotten.version_id);
        journal.append(2, "Evidence still retained");
        let after = journal.collect(&cache, &tombstones);
        let cold = journal.collect(&CaptureCacheInput::default(), &tombstones);
        assert_eq!(semantics(&after), semantics(&cold));
        assert_eq!(after.items.len(), 1);
        assert_eq!(after.work.parsed_values, 1);
        assert_eq!(
            after.work.verified_prefix_bytes,
            first.cursors[0].2["byte_offset"]
        );
        let candidate = after.cache_input();
        assert_eq!(candidate.referenced_versions().len(), 1);
        assert!(
            !candidate
                .referenced_versions()
                .contains(&forgotten.version_id)
        );
        assert!(
            !candidate
                .payloads_by_version
                .contains_key(&forgotten.version_id)
        );
        assert!(
            !serde_json::to_string(&after.cursors)
                .unwrap()
                .contains("Original captured evidence")
        );
        let repeated = journal.collect(&candidate, &tombstones);
        assert_eq!(semantics(&repeated), semantics(&after));
        assert_eq!(repeated.work.parsed_values, 0);
        assert_eq!(repeated.work.reused_records, 1);
    }
}

#[test]
fn incomplete_or_unterminated_files_supply_no_reusable_cache() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let first = journal.cold();
        let prefix = fs::metadata(&journal.path).unwrap().len();
        write!(
            fs::OpenOptions::new()
                .append(true)
                .open(&journal.path)
                .unwrap(),
            "{{\"type\":"
        )
        .unwrap();
        let broken = journal.collect(&first.cache_input(), &[]);
        assert_eq!(semantics(&broken), semantics(&journal.cold()));
        assert!(broken.incomplete);
        assert_eq!(broken.cursors[0].2["byte_offset"], prefix);
        assert_eq!(broken.cursors[0].2["complete"], false);
        assert!(broken.cache_input().cursors.is_empty());
        journal.replace(&[(1, "Original captured evidence")]);
        let bytes = fs::read(&journal.path).unwrap();
        fs::write(&journal.path, &bytes[..bytes.len() - 1]).unwrap();
        let unterminated = journal.cold();
        assert_eq!(unterminated.items.len(), 1);
        assert!(unterminated.cache_input().cursors.is_empty());
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&journal.path)
            .unwrap();
        writeln!(file).unwrap();
        drop(file);
        journal.append(2, "A correctly delimited append");
        let recovered = journal.collect(&broken.cache_input(), &[]);
        assert_eq!(semantics(&recovered), semantics(&journal.cold()));
        assert_eq!(recovered.items.len(), 2);
        assert_eq!(recovered.cache_input().referenced_versions().len(), 2);
    }
}

#[test]
fn cached_scope_is_resolved_again_and_unknown_scope_remains_incomplete() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let cwd = journal.fixture.root.join("session-work");
        fs::create_dir(&cwd).unwrap();
        let mut header = journal.header();
        if kind == "omp" {
            header["cwd"] = json!(cwd);
        } else {
            header["payload"]["cwd"] = json!(cwd);
        }
        fs::write(
            &journal.path,
            format!("{header}\n{}\n", journal.record(1, "Scoped evidence")),
        )
        .unwrap();
        let first = journal.cold();
        assert_eq!(first.items.len(), 1);
        let cache = first.cache_input();
        fs::remove_dir(&cwd).unwrap();
        let unresolved = journal.collect(&cache, &[]);
        assert_eq!(semantics(&unresolved), semantics(&journal.cold()));
        assert!(unresolved.incomplete);
        assert!(unresolved.items.is_empty());
        assert!(
            unresolved.coverage[&format!("harness-{kind}")]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["reason"] == "scope_unresolved")
        );
        assert_ne!(unresolved.inventory, first.inventory);
        assert!(unresolved.cache_input().cursors.is_empty());
        assert_eq!(unresolved.work.scope_discoveries, 1);
        fs::create_dir(&cwd).unwrap();
        let restored = journal.collect(&cache, &[]);
        assert_eq!(semantics(&restored), semantics(&first));
        assert_eq!(restored.work.reused_records, 1);

        support::git(&cwd, &["init", "-q", "-b", "main"]);
        let unrelated = journal.collect(&cache, &[]);
        assert_eq!(semantics(&unrelated), semantics(&journal.cold()));
        assert!(!unrelated.incomplete);
        assert!(unrelated.items.is_empty());
        assert_eq!(unrelated.work.reused_records, 0);
        assert_ne!(unrelated.inventory, first.inventory);
    }
}

#[test]
fn overlapping_caches_still_coalesce_and_conflicts_pause_affected_files() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let copy = journal.path.with_file_name("rotated.jsonl");
        fs::copy(&journal.path, &copy).unwrap();
        let first = journal.cold();
        assert_eq!(first.items.len(), 1);
        assert_eq!(first.work.scope_discoveries, 1);
        assert_eq!(first.cache_input().referenced_versions().len(), 1);
        journal.append(2, "Record in the active copy");
        let overlap = journal.collect(&first.cache_input(), &[]);
        assert_eq!(semantics(&overlap), semantics(&journal.cold()));
        assert_eq!(overlap.items.len(), 2);
        assert_eq!(overlap.work.parsed_values, 1);
        assert_eq!(overlap.work.reused_records, 2);
        assert_eq!(overlap.work.scope_discoveries, 1);

        fs::write(
            &copy,
            format!(
                "{}\n{}\n",
                journal.header(),
                journal.record(1, "Conflicting copy")
            ),
        )
        .unwrap();
        let conflicting = journal.collect(&overlap.cache_input(), &[]);
        assert_eq!(semantics(&conflicting), semantics(&journal.cold()));
        assert!(conflicting.incomplete);
        assert_eq!(conflicting.items.len(), 1);
        assert!(conflicting.cache_input().cursors.is_empty());
        assert!(conflicting.cache_input().referenced_versions().is_empty());
        for (_, _, cursor) in &conflicting.cursors {
            assert_eq!(cursor["complete"], false);
            assert_eq!(cursor["byte_offset"], 0);
            assert!(cursor.get("cache").is_none());
        }
    }
}

#[test]
fn credential_exclusions_preserve_inventory_without_caching_bodies() {
    let journal = Journal::new("omp");
    let first = journal.cold();
    let secret = "sk-syntheticcredentialvalue1234567890";
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(&journal.path)
            .unwrap(),
        "{}",
        json!({"type":"credential_pin","id":"binding","credential":secret})
    )
    .unwrap();
    journal.append(2, "Evidence after excluded credentials");
    let appended = journal.collect(&first.cache_input(), &[]);
    let candidate = appended.cache_input();
    let repeated = journal.collect(&candidate, &[]);
    assert_eq!(semantics(&repeated), semantics(&journal.cold()));
    assert_eq!(repeated.work.parsed_values, 0);
    assert_eq!(repeated.items.len(), 2);
    assert_eq!(candidate.referenced_versions().len(), 2);
    assert!(
        !serde_json::to_string(&appended.cursors)
            .unwrap()
            .contains(secret)
    );
}

#[test]
fn public_publication_and_store_loader_supply_a_usable_committed_cache() {
    for kind in ["omp", "codex"] {
        let journal = Journal::new(kind);
        let run = |args: &[&str]| {
            let output = journal
                .fixture
                .command()
                .env_remove("MEMQ_OMP_STORE")
                .env_remove("MEMQ_CODEX_STORE")
                .env_remove("MEMQ_OPENCODE_STORE")
                .env_remove("MEMQ_CODE_REPORT")
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "synthetic CLI command failed");
            output.stdout
        };
        run(&["init"]);
        journal.fixture.add_source(&format!(
            "[capture]\n{kind}={}",
            json!(journal.path.parent().unwrap())
        ));
        let raw = run(&[
            "brief",
            "--compact",
            "--budget-kind",
            "bytes",
            "--budget",
            "30000",
        ]);
        support::budget_check(&raw);
        let response: Value = serde_json::from_slice(&raw).unwrap();
        let items = support::expanded_items(&response);
        assert_eq!(items.len(), 1);
        let config = memq::config::Config::parse(
            &fs::read_to_string(journal.fixture.root.join(".memq/config.toml")).unwrap(),
        )
        .unwrap();
        let store =
            memq::store::Store::open(&journal.fixture.store_root(), &journal.repo.clone_id, false)
                .unwrap();
        let cache = store.capture_cache().unwrap();
        assert_eq!(cache.cursors.len(), 1);
        assert_eq!(cache.referenced_versions().len(), 1);
        assert_eq!(cache.payloads_by_version.len(), 1);
        let warm = capture::collect(
            &journal.repo,
            &config.project_id,
            &config.capture,
            &[],
            &cache,
        )
        .unwrap();
        assert_eq!(warm.items.len(), 1);
        assert_eq!(warm.items[0].id, items[0]["id"]);
        assert_eq!(warm.items[0].payload.redacted_text, items[0]["text"]);
        assert!(
            cache
                .payloads_by_version
                .contains_key(&warm.items[0].version_id)
        );
        assert_eq!(warm.work.parsed_values, 0);
        assert_eq!(warm.work.redacted_records, 0);
        assert_eq!(warm.work.reused_records, 1);
        assert_eq!(
            warm.work.verified_prefix_bytes,
            fs::metadata(&journal.path).unwrap().len()
        );
    }
}
