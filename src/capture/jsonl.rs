//! Shared JSONL inventory, tail recovery, identity deduplication, and redaction.
use super::jsonl_cache::{Binding, Entry, EntryState, Manifest};
use super::{
    CaptureCacheInput, CaptureWork, Captured, ScopeCache, codex, make_item, omp, session,
    validate_metadata,
};
use crate::error::{Error, Result};
use crate::records::Item;
use crate::redact;
use crate::repository::Repository;
use crate::tombstone::{self, Tombstone};
use crate::util;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

pub(super) fn coalesce(
    out: &mut Captured,
    before: usize,
    source: &str,
    errors: &mut Vec<Value>,
) -> usize {
    let mut groups: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for item in out.items.drain(before..) {
        groups.entry(item.id.clone()).or_default().push(item);
    }
    let mut coalesced = 0;
    let mut paused_paths = BTreeSet::new();
    for (id, copies) in groups {
        // File inventories are sorted. Keep the first lossless pointer and its
        // exact byte range; content equality alone cannot resolve scope changes.
        let first = &copies[0];
        let observation = &first.observation;
        let agrees = copies.iter().all(|item| {
            item.version_id == first.version_id
                && item.observation.worktree_key == observation.worktree_key
                && item.observation.branch == observation.branch
                && item.observation.native_revision == observation.native_revision
        });
        if agrees {
            coalesced += copies.len() - 1;
            out.items
                .push(copies.into_iter().next().expect("nonempty group"));
        } else {
            let paths: BTreeSet<_> = copies
                .iter()
                .map(|item| item.observation.source_path.clone())
                .collect();
            errors.push(
                json!({"reason":"capture_conflict","item_id":id,"paths":paths,
                "detail":"conflicting native record copies; identity paused"}),
            );
            paused_paths.extend(paths);
        }
    }
    if !paused_paths.is_empty() {
        // Let reconciliation retain its last usable view if nothing else can
        // be published. Otherwise publish only unambiguous evidence, incomplete.
        out.missing
            .push(json!({"id":source,"reason":"capture_conflict"}));
        for (cursor_source, path, cursor) in &mut out.cursors {
            if cursor_source == source && paused_paths.contains(path) {
                // A conflict has no accepted winner. Replay affected files from
                // zero rather than committing progress past rejected evidence.
                cursor["byte_offset"] = json!(0);
                cursor["last_id_or_ordinal"] = Value::Null;
                cursor["complete"] = json!(false);
                cursor.as_object_mut().expect("cursor").remove("cache");
                cursor
                    .as_object_mut()
                    .expect("cursor")
                    .remove("cache_sha256");
            }
        }
    }
    coalesced
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect(
    repo: &Repository,
    project: &str,
    kind: &str,
    root: &Path,
    path: &Path,
    tombstones: &[Tombstone],
    cache: &CaptureCacheInput,
    scopes: &mut ScopeCache,
    out: &mut Captured,
) -> Result<()> {
    let source = format!("harness-{kind}");
    let relative = path
        .strip_prefix(root)
        .expect("inside root")
        .to_string_lossy()
        .into_owned();
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let binding = Binding::new(repo, project, kind, root, &relative, &metadata)?;
    let mut first = Vec::new();
    (&mut file).take(4096).read_to_end(&mut first)?;
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut offset = 0u64;
    let mut accepted_offset = 0u64;
    let mut last = Value::Null;
    let mut session_info = None;
    let mut hasher = Sha256::new();
    let mut error = None;
    let mut ids = BTreeSet::new();
    let mut ordinal_before = None;
    let mut header_end = 0;
    let mut header_ordinal = None;
    let mut entries = Vec::new();
    let mut ends_with_newline = false;
    if let Some(mut saved) = cache
        .cursors
        .get(&(source.clone(), relative.clone()))
        .and_then(Manifest::decode)
        .filter(|saved| saved.binding == binding && saved.prefix_len <= metadata.len())
        && let Some(proof) = verify_prefix(
            &mut reader,
            saved.prefix_len,
            &saved.prefix_sha256,
            &mut out.work,
        )?
        && let Some(items) = saved.restore(repo, cache, tombstones, scopes, &mut out.work)
    {
        // No source values or payload bodies were parsed during verification.
        // Reconstructed observations keep the exact native ranges and pointers.
        reader.seek(SeekFrom::Start(saved.prefix_len))?;
        offset = saved.prefix_len;
        accepted_offset = offset;
        hasher = proof;
        header_end = saved.header_end;
        header_ordinal = saved.header_ordinal;
        ordinal_before = if kind == "codex" {
            saved
                .entries
                .last()
                .and_then(|e| e.id.parse().ok())
                .or(header_ordinal)
        } else {
            header_ordinal
        };
        last = json!(saved.entries.last().map(|entry| entry.id.as_str()));
        ids.extend(saved.entries.iter().map(|entry| entry.id.clone()));
        out.inventory.push(json!([
            source,
            relative,
            "scope",
            util::hash_json(&saved.session)?
        ]));
        for entry in &saved.entries {
            if matches!(entry.state, EntryState::CredentialPin) {
                out.inventory.push(json!([
                    source,
                    relative,
                    entry.id,
                    "excluded:credential_pin"
                ]));
            }
        }
        out.work.reused_records += items.len() as u64;
        out.items.extend(items);
        session_info = Some(saved.session);
        entries = saved.entries;
        ends_with_newline = true;
    } else {
        // Verification, scope, and payload failures are recoverable misses.
        reader.seek(SeekFrom::Start(0))?;
    }
    while reader.read_until(b'\n', &mut line)? > 0 {
        let begin = offset;
        offset += line.len() as u64;
        ends_with_newline = line.ends_with(b"\n");
        hasher.update(&line);
        out.work.parsed_values += 1;
        let record: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                error = Some(Error::new(
                    if line.ends_with(b"\n") {
                        "capture_schema"
                    } else {
                        "incomplete_tail"
                    },
                    "unreadable JSONL value; cursor not advanced",
                ));
                line.clear();
                break;
            }
        };
        let typ = record["type"].as_str().unwrap_or("");
        if session_info.is_none() {
            let data = match if kind == "omp" {
                omp::header(&record)
            } else {
                codex::header(&record)
            } {
                Ok(Some(data)) => data,
                Ok(None) => {
                    line.clear();
                    continue;
                }
                Err(e) => {
                    error = Some(e);
                    line.clear();
                    break;
                }
            };
            session_info = match session(repo, data, scopes, &mut out.work) {
                Ok(Some(s)) => Some(s),
                Ok(None) => {
                    out.inventory
                        .push(json!([source, relative, "unrelated_git_repository"]));
                    return Ok(());
                }
                Err(e) => {
                    error = Some(e);
                    line.clear();
                    break;
                }
            };
            // Scope is semantic input even when all file bytes stay unchanged.
            // Both replay and reuse emit the same witness for publication.
            out.inventory.push(json!([
                source,
                relative,
                "scope",
                util::hash_json(&session_info)?
            ]));
            accepted_offset = offset;
            header_end = offset;
            ordinal_before = record["ordinal"].as_u64();
            header_ordinal = ordinal_before;
            line.clear();
            continue;
        }
        let s = session_info.as_ref().expect("header checked");
        let entry = match if kind == "omp" {
            omp::entry(&record)
        } else {
            codex::entry(&record, &mut ordinal_before)
        } {
            Ok(entry) => entry,
            Err(e) => {
                error = Some(e);
                line.clear();
                break;
            }
        };
        if let Err(e) = validate_metadata(&entry, "entry_id") {
            error = Some(e);
            line.clear();
            break;
        }
        if !ids.insert(entry.clone()) {
            error = Some(Error::new(
                "capture_schema",
                "duplicate session entry identity",
            ));
            line.clear();
            break;
        }
        if typ == "credential_pin" {
            // Credential bindings are explicitly excluded and counted, never persisted.
            out.inventory
                .push(json!([source, relative, entry, "excluded:credential_pin"]));
            entries.push(Entry {
                id: entry.clone(),
                byte_range: [begin, offset],
                source_sha256: util::hash(&line),
                recorded_at: None,
                state: EntryState::CredentialPin,
            });
            accepted_offset = offset;
            last = json!(entry);
            line.clear();
            continue;
        }
        let pointer = if kind == "codex" {
            codex::pointer(&relative, &entry)
        } else {
            omp::pointer(&relative, &entry)
        };
        out.work.redacted_records += 1;
        let redacted = redact::value(&record);
        let text = String::from_utf8(util::canonical(&redacted)?).expect("JSON");
        let mut item = make_item(
            project,
            &source,
            s,
            &entry,
            text,
            &relative,
            pointer,
            [begin, offset],
            &record,
            util::hash(&line),
        )?;
        item.observation.source_root = Some(root.to_string_lossy().into_owned());
        let excluded = tombstone::excludes_record(
            tombstones,
            &cache.forgotten_ids,
            &item.id,
            &item.source_id,
            item.observation.recorded_at.as_deref(),
        );
        entries.push(Entry {
            id: entry.clone(),
            byte_range: [begin, offset],
            source_sha256: item.observation.source_sha256.clone(),
            recorded_at: item.observation.recorded_at.clone(),
            state: if excluded {
                EntryState::Tombstoned
            } else {
                EntryState::Retained {
                    version_id: item.version_id.clone(),
                }
            },
        });
        if !excluded {
            out.items.push(item);
        }
        accepted_offset = offset;
        last = json!(entry);
        line.clear();
    }
    // Complete inventory hashing detects replacements and changed bytes before
    // cursors are reused, including edits outside any timestamp lookback.
    let mut buffer = [0u8; 65536];
    loop {
        let n = reader.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let fingerprint = format!("{:x}", hasher.finalize());
    out.inventory
        .push(json!([source, relative, fingerprint, metadata.len()]));
    if session_info.is_none() && error.is_none() {
        error = Some(Error::new("capture_schema", "missing session header"));
    }
    let mut cursor = json!({
        "fingerprint":{"first_4k_sha256":util::hash(first),"inode":binding.inode,"device":binding.device},
        "byte_offset":accepted_offset,"last_id_or_ordinal":last,"size":metadata.len(),
        "mtime":metadata.modified().ok().and_then(|t|t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d|d.as_nanos().to_string()),
        "complete":error.is_none(),"inventory_sha256":fingerprint
    });
    // Only a complete, newline-delimited prefix can be resumed. A partial or
    // paused scan keeps its existing accepted cursor but provides no cache.
    if error.is_none() && ends_with_newline && accepted_offset == metadata.len() {
        Manifest::new(
            binding,
            accepted_offset,
            fingerprint,
            session_info.expect("successful header"),
            header_end,
            header_ordinal,
            entries,
        )
        .write(&mut cursor)?;
    }
    out.cursors.push((source, relative, cursor));
    if let Some(error) = error {
        return Err(error);
    }
    Ok(())
}

fn verify_prefix(
    reader: &mut BufReader<File>,
    length: u64,
    expected: &str,
    work: &mut CaptureWork,
) -> Result<Option<Sha256>> {
    let mut hasher = Sha256::new();
    let mut remaining = length;
    let mut buffer = [0; 65536];
    let mut last = 0;
    while remaining > 0 {
        let limit = remaining.min(buffer.len() as u64) as usize;
        let n = reader.read(&mut buffer[..limit])?;
        if n == 0 {
            return Ok(None);
        }
        work.verified_prefix_bytes += n as u64;
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
        last = buffer[n - 1];
    }
    Ok((last == b'\n' && format!("{:x}", hasher.clone().finalize()) == expected).then_some(hasher))
}
