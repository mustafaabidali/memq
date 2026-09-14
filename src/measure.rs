//! Explicit, isolated development measurements. No production store is opened.
use crate::error::{Error, Result};
use crate::{records, util};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

fn timed<T>(f: impl FnOnce() -> Result<T>) -> Result<(T, f64)> {
    let start = Instant::now();
    let result = f()?;
    Ok((result, ms(start)))
}

fn sqlite(root: &Path, count: usize, detail: &str) -> Result<Value> {
    let path = root.join(format!("trigram-{detail}.sqlite"));
    let db = Connection::open(&path)?;
    db.execute_batch(&format!(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;
         CREATE VIRTUAL TABLE corpus USING fts5(body, tokenize='trigram', detail={detail});
         INSERT INTO corpus(corpus,rank) VALUES('secure-delete',1);"
    ))?;
    let (_, build) = timed(|| {
        let tx = db.unchecked_transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO corpus(rowid,body) VALUES(?1,?2)")?;
            for n in 0..count {
                let body = format!(
                    "Record {n} callback authorization phone before email. \
                     apps/web/src/auth/callback_{n}.rs الهاتف قبل البريد الإلكتروني."
                );
                insert.execute(params![n as i64 + 1, records::normalized(&body)])?;
            }
        }
        tx.commit()?;
        Ok(())
    })?;
    let (_, save) = timed(|| {
        db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        fs::File::open(&path)?.sync_all()?;
        Ok(())
    })?;
    let mut queries = Vec::new();
    for (name, sql, value) in [
        (
            "match_long_token",
            "SELECT count(*) FROM corpus WHERE corpus MATCH ?1",
            "\"callback\"",
        ),
        (
            "literal_glob",
            "SELECT count(*) FROM corpus WHERE body GLOB ?1",
            "*callback*",
        ),
        (
            "short_token",
            "SELECT count(*) FROM corpus WHERE corpus MATCH ?1",
            "\"ab\"",
        ),
        (
            "arabic_match",
            "SELECT count(*) FROM corpus WHERE corpus MATCH ?1",
            "\"الهاتف\"",
        ),
    ] {
        let start = Instant::now();
        let result = db.query_row(sql, [value], |r| r.get::<_, i64>(0));
        queries.push(match result {
            Ok(found) => json!({"query":name,"status":"ok","matches":found,"ms":ms(start)}),
            Err(e) if detail != "full" && matches!(name, "match_long_token" | "arabic_match") =>
                json!({"query":name,"status":"unsupported_for_detail_mode","error":e.to_string(),"ms":ms(start)}),
            Err(e) => return Err(e.into()),
        });
    }
    db.execute_batch("INSERT INTO corpus(corpus) VALUES('integrity-check');")?;
    let size = fs::metadata(&path)?.len();
    drop(db);
    let (_, load) = timed(|| {
        let db = Connection::open(&path)?;
        let actual: usize = db.query_row("SELECT count(*) FROM corpus", [], |r| r.get(0))?;
        if actual != count {
            return Err(Error::new(
                "measurement_failed",
                "SQLite load count mismatch",
            ));
        }
        Ok(())
    })?;
    Ok(
        json!({"detail":detail,"records":count,"build_ms":build,"save_checkpoint_ms":save,
        "reopen_and_count_ms":load,"bytes":size,"queries":queries}),
    )
}

fn vectors(root: &Path, count: usize) -> Result<Value> {
    let dims = 384;
    let options = IndexOptions {
        dimensions: dims,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: 0,
        expansion_add: 0,
        expansion_search: 0,
        multi: false,
    };
    let engine = |_| Error::new("measurement_failed", "USearch error");
    let index = Index::new(&options).map_err(engine)?;
    index.reserve(count).map_err(engine)?;
    let synthetic: Vec<Vec<f32>> = (0..count)
        .map(|n| {
            (0..dims)
                .map(|d| ((n * 7919 + d * 31) as f32).sin())
                .collect()
        })
        .collect();
    let (_, build) = timed(|| {
        for (i, v) in synthetic.iter().enumerate() {
            index.add(i as u64, v.as_slice()).map_err(engine)?;
        }
        Ok(())
    })?;
    let path = root.join("mechanical.usearch");
    let filename = path
        .to_str()
        .ok_or_else(|| Error::new("measurement_failed", "non-UTF-8 temporary directory"))?;
    let (_, save) = timed(|| {
        index.save(filename).map_err(engine)?;
        fs::File::open(&path)?.sync_all()?;
        Ok(())
    })?;
    let loaded = Index::new(&options).map_err(engine)?;
    let metadata = Index::metadata(filename).map_err(engine)?;
    let (_, load) = timed(|| {
        loaded.load(filename).map_err(engine)?;
        if loaded.size() != count {
            return Err(Error::new(
                "measurement_failed",
                "USearch load count mismatch",
            ));
        }
        Ok(())
    })?;
    let mut query_ms = Vec::new();
    for vector in synthetic.iter().take(20) {
        let (result, elapsed) = timed(|| loaded.search(vector.as_slice(), 10).map_err(engine))?;
        if result.keys.is_empty() {
            return Err(Error::new(
                "measurement_failed",
                "USearch returned no self candidates",
            ));
        }
        query_ms.push(elapsed);
    }
    Ok(
        json!({"corpus":"deterministic mechanical vectors; no semantic validation",
        "writer_version":format!("{}.{}.{}", metadata.version_major, metadata.version_minor, metadata.version_patch),
        "dimensions":dims,"quantization":"F32","records":count,"build_ms":build,"save_ms":save,
        "load_ms":load,"bytes":fs::metadata(path)?.len(),"query_ms":query_ms,
        "connectivity":index.connectivity(),"expansion_add":index.expansion_add(),
        "expansion_search":index.expansion_search()}),
    )
}

fn tokenizers() -> Result<Value> {
    let mut out = Vec::new();
    for name in ["o200k_base", "cl100k_base"] {
        let start = Instant::now();
        let tokenizer = match name {
            "o200k_base" => tiktoken_rs::o200k_base(),
            _ => tiktoken_rs::cl100k_base(),
        }
        .map_err(|_| Error::new("measurement_failed", "tokenizer initialization failed"))?;
        let init = ms(start);
        let mut corpora = Vec::new();
        for (kind, text) in [
            (
                "english",
                "Preserve the original pointer, current approval and reported verification.",
            ),
            (
                "arabic",
                "التحقق من الملفات لا يثبت أن الاختبار نجح. يجب حفظ الأمر والإصدار والبيئة.",
            ),
            (
                "punctuation",
                "mq:01ARZ3NDEKTSV4RRFFQ69G5FAV:source:abc%3Adef apps/web/[locale]/callback.ts {\"x\":null}",
            ),
        ] {
            let payload = serde_json::to_string(&json!({
                "memq":{"envelope":1,"operation":"brief"},"items":[{"text":text.repeat(32)}],
                "scope":{"task":"synthetic"},"freshness":{"status":"current"},
                "budget":{"limit":4000,"kind":"tokens","tokenizer":name},
                "omitted_count":0,"continuation":null
            }))?;
            let start = Instant::now();
            let mut tokens = 0;
            for _ in 0..100 {
                tokens = tokenizer
                    .encode_ordinary(std::hint::black_box(&payload))
                    .len();
            }
            corpora.push(json!({"kind":kind,"bytes":payload.len(),"tokens":tokens,
                "encode_ms_mean":ms(start)/100.0,"iterations":100}));
        }
        out.push(json!({"tokenizer":name,"initialize_ms":init,"corpora":corpora}));
    }
    Ok(json!(out))
}

#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    // getrusage initializes the provided POD struct on success.
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    Some(usage.ru_maxrss as u64 * if cfg!(target_os = "macos") { 1 } else { 1024 })
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

pub fn run(count: usize) -> Result<Value> {
    if !(10..=100_000).contains(&count) {
        return Err(Error::new(
            "invalid_request",
            "measurement records must be 10..100000",
        ));
    }
    let root = std::env::temp_dir().join(format!("memq-measure-{}", util::id()));
    fs::create_dir(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    let scratch = Scratch(root);
    let sql = ["full", "column", "none"]
        .iter()
        .map(|mode| sqlite(&scratch.0, count, mode))
        .collect::<Result<Vec<_>>>()?;
    let vectors = vectors(&scratch.0, count)?;
    let tokenizers = tokenizers()?;
    Ok(json!({
        "format":1,"experiment":"storage-tokenizer-trigram","parameters":"provisional",
        "os":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "sqlite_version":rusqlite::version(),
        "records":count,"trigram":sql,"vectors":vectors,"tokenizers":tokenizers,
        "peak_process_rss_bytes":peak_rss_bytes(),
        "cache_conditions":"Fresh isolated files; OS page cache not flushed. Load and repeated queries are warm.",
        "limitations":["Mechanical storage and tokenizer costs, not semantic quality.",
            "End-to-end reconciliation is measured separately on the workflow fixture."]
    }))
}
