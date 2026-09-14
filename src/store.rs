use crate::error::{Error, Result};
use crate::policy::Member;
use crate::records::{Item, Payload};
use crate::util;
use fs2::FileExt;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

mod embeddings;
pub(crate) use embeddings::VectorUpdate;

mod schema;
pub use schema::SCHEMA;
use schema::{
    compatible, create_schema, has_table, history_layout, index_layout, migrate_index,
    schema_version,
};
pub(crate) use schema::{empty_build_database, purge_layout};

pub struct MutationLock {
    _file: File,
}

impl MutationLock {
    pub fn acquire(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("mutation.lock"))?;
        file.try_lock_exclusive().map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                Error::new("in_progress", "a live writer owns the clone mutation lock")
            } else {
                e.into()
            }
        })?;
        Ok(Self { _file: file })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub id: String,
    pub scope_hash: String,
    pub inputs_hash: String,
    pub meta: Value,
    pub members: Vec<Member>,
}

pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) generation: String,
    pub(crate) db: Connection,
    pub(crate) access: Connection,
    pub(crate) replacement: bool,
    pub(crate) previous: Option<Connection>,
}

struct HistoricalMember {
    member: Member,
    view_id: String,
    meta: Value,
    row: i64,
    publication_seq: Option<i64>,
    generation: String,
}

fn publication_column(db: &Connection) -> Result<&'static str> {
    if db
        .prepare("SELECT publication_seq FROM views LIMIT 0")
        .is_ok()
    {
        Ok("publication_seq")
    } else if schema_version(db)? >= 3 {
        Err(Error::new(
            "history_order_unavailable",
            "retained publication sequence is missing",
        ))
    } else {
        // A retained pre-v3 generation is still readable without migrating it.
        Ok("NULL")
    }
}

fn historical_members(db: &Connection, id: &str) -> Result<Vec<HistoricalMember>> {
    let mut stmt = db.prepare(&format!(
        "SELECT vi.member_json,vi.view_id,v.meta_json,v.rowid,{} FROM view_items vi
         JOIN views v ON v.view_id=vi.view_id WHERE vi.item_id=?1 ORDER BY v.rowid DESC",
        publication_column(db)?,
    ))?;
    let mut out = Vec::new();
    for row in stmt.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })? {
        let (member, view_id, meta, row, publication_seq) = row?;
        out.push(HistoricalMember {
            member: serde_json::from_str(&member)?,
            view_id,
            meta: serde_json::from_str(&meta)?,
            row,
            publication_seq,
            generation: db.path().expect("on-disk store").to_owned(),
        });
    }
    Ok(out)
}

pub fn data_root(clone_id: &str) -> Result<PathBuf> {
    let base = if let Some(p) = std::env::var_os("MEMQ_DATA_DIR") {
        PathBuf::from(p)
    } else if let Some(p) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(p).join("memq")
    } else {
        PathBuf::from(
            std::env::var_os("HOME")
                .ok_or_else(|| Error::new("invalid_config", "set MEMQ_DATA_DIR"))?,
        )
        .join(".local/share/memq")
    };
    Ok(base.join(clone_id))
}

fn connection(path: &Path, create: bool) -> Result<Connection> {
    let mut flags = OpenFlags::default();
    if !create {
        flags.remove(OpenFlags::SQLITE_OPEN_CREATE);
    }
    let db = Connection::open_with_flags(path, flags)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; PRAGMA synchronous=FULL; PRAGMA journal_mode=WAL;")?;
    Ok(db)
}

fn healthy(db: &Connection) -> Result<bool> {
    Ok(db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? == "ok")
}

fn corrupt_error(e: &Error) -> bool {
    e.code == "storage_error"
        && e.detail["message"].as_str().is_some_and(|s| {
            s.contains("not a database") || s.contains("malformed") || s.contains("corrupt")
        })
}

impl Store {
    pub(crate) fn sqlite_info(&self) -> Result<Value> {
        schema::sqlite_info(&self.db)
    }

    /// Caller holds the stable mutation lock throughout opening and publication.
    pub fn open(root: &Path, clone_id: &str, rebuild: bool) -> Result<Self> {
        let access_path = root.join("access.sqlite");
        let marker = root.join("CURRENT");
        let generations = root.join("generations");
        let access_exists = access_path.try_exists()?;
        let initialized = marker.try_exists()?
            || (generations.try_exists()?
                && fs::read_dir(&generations)?.next().transpose()?.is_some());
        if !access_exists && initialized {
            return Err(Error::new(
                "access_state_incomplete",
                "the persistent deletion ledger is missing; restore it before replay",
            ));
        }
        if access_exists {
            let inspect =
                Connection::open_with_flags(&access_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            compatible(&inspect)?;
            if !healthy(&inspect)? {
                return Err(Error::new(
                    "access_state_corrupt",
                    "restore the content-free deletion ledger before replay",
                ));
            }
            let version = schema_version(&inspect)?;
            if (initialized || version > 0) && !has_table(&inspect, "tombstones")? {
                return Err(Error::new(
                    "access_state_incomplete",
                    "the deletion ledger is missing; replay is blocked to prevent resurrection",
                ));
            }
            if version >= 2
                && inspect
                    .prepare("SELECT item_id FROM forgotten_items LIMIT 0")
                    .is_err()
            {
                return Err(Error::new(
                    "access_state_incomplete",
                    "persistent forgotten identities are missing; restore the access ledger before replay",
                ));
            }
            if version >= 3
                && (inspect.prepare("SELECT project_id,key,item_id,semantic_sha256,legacy_key FROM forgotten_note_ops LIMIT 0").is_err()
                    || inspect.query_row("SELECT sequence FROM publication_clock WHERE singleton=1", [], |r| r.get::<_, i64>(0)).is_err())
            {
                return Err(Error::new(
                    "access_state_incomplete",
                    "persistent note identities or publication order are missing; restore the access ledger",
                ));
            }
        }
        fs::create_dir_all(&generations)?;
        let access = connection(&access_path, !access_exists && !initialized)?;
        compatible(&access)?;
        // Additive repair preserves all existing rows. This migrates the
        // reviewed M1 cache (schema 1) and repairs absent reconstructible tables.
        schema::migrate_access(&access)?;
        let mut previous = None;
        if marker.exists()
            && let Ok(generation) = fs::read_to_string(&marker)
            && ulid::Ulid::from_string(generation.trim()).is_ok()
        {
            let generation = generation.trim().to_owned();
            let path = root
                .join("generations")
                .join(&generation)
                .join("index.sqlite");
            if path.exists() {
                // Inspect the schema before executing any migration or replacement.
                let inspect = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                match compatible(&inspect).and_then(|()| healthy(&inspect)) {
                    Ok(true) => {
                        let version = schema_version(&inspect)?;
                        if !matches!(version, 1 | 2 | SCHEMA) {
                            return Err(Error::new(
                                "migration_failed",
                                "no migration exists for this store version",
                            ));
                        }
                        let layout = index_layout(&inspect)?;
                        if history_layout(&inspect) {
                            // A selected legacy view was verified when published.
                            // Preserve its discoverability before replacing CURRENT.
                            util::atomic_replace(
                                &path.parent().expect("generation").join("VERIFIED"),
                                b"1",
                            )?;
                        }
                        drop(inspect);
                        if layout {
                            let db = connection(&path, false)?;
                            migrate_index(&db)?;
                            if !rebuild {
                                return Ok(Self {
                                    root: root.into(),
                                    generation,
                                    db,
                                    access,
                                    replacement: false,
                                    previous: None,
                                });
                            }
                            previous = Some(db);
                        } else if history_layout(&Connection::open_with_flags(
                            &path,
                            OpenFlags::SQLITE_OPEN_READ_ONLY,
                        )?) {
                            previous = Some(Connection::open_with_flags(
                                &path,
                                OpenFlags::SQLITE_OPEN_READ_ONLY,
                            )?);
                        }
                    }
                    Ok(false) => (),
                    Err(e) if corrupt_error(&e) => (),
                    Err(e) => return Err(e),
                }
            }
        }
        let generation = util::id();
        let directory = root.join("generations").join(&generation);
        fs::create_dir(&directory)?;
        let db = connection(&directory.join("index.sqlite"), true)?;
        util::fault("after_generation_open");
        create_schema(&db, clone_id)?;
        if previous.is_none() {
            previous = archive_connections(root, &generation)?.into_iter().next();
        }
        Ok(Self {
            root: root.into(),
            generation,
            db,
            access,
            replacement: true,
            previous,
        })
    }

    pub fn current(&self, scope_hash: &str) -> Result<Option<View>> {
        let id: Option<String> = self
            .db
            .query_row(
                "SELECT view_id FROM views WHERE scope_hash=?1 AND is_current=1",
                [scope_hash],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| load_view(&self.db, &id)).transpose()
    }

    pub fn previous_view(&self, scope_hash: &str) -> Result<Option<View>> {
        if let Some(view) = self.current(scope_hash)? {
            return Ok(Some(view));
        }
        if let Some(db) = &self.previous {
            let id: Option<String> = db
                .query_row(
                    "SELECT view_id FROM views WHERE scope_hash=?1 AND is_current=1",
                    [scope_hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                return load_view(db, &id).map(Some);
            }
        }
        for db in archive_connections(&self.root, &self.generation)? {
            let id: Option<String> = db
                .query_row(
                    "SELECT view_id FROM views WHERE scope_hash=?1 AND is_current=1",
                    [scope_hash],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                return load_view(&db, &id).map(Some);
            }
        }
        Ok(None)
    }

    pub fn view(&self, id: &str) -> Result<View> {
        self.with_view_db(id, |db| load_view(db, id))
    }

    pub fn payload(&self, version: &str) -> Result<Payload> {
        let json: Option<String> = self
            .db
            .query_row(
                "SELECT payload_json FROM item_versions WHERE version_id=?1",
                [version],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = json {
            return serde_json::from_str(&json).map_err(Into::into);
        }
        for db in archive_connections(&self.root, &self.generation)? {
            let json: Option<String> = db
                .query_row(
                    "SELECT payload_json FROM item_versions WHERE version_id=?1",
                    [version],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(json) = json {
                return serde_json::from_str(&json).map_err(Into::into);
            }
        }
        Err(Error::new(
            "missing_evidence",
            "retained version unavailable",
        ))
    }

    pub fn with_view_db<T>(&self, id: &str, read: impl Fn(&Connection) -> Result<T>) -> Result<T> {
        let exists: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM views WHERE view_id=?1)",
            [id],
            |r| r.get(0),
        )?;
        if exists {
            return read(&self.db);
        }
        for db in archive_connections(&self.root, &self.generation)? {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM views WHERE view_id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if exists {
                return read(&db);
            }
        }
        Err(Error::new(
            "stale_continuation",
            "retained view unavailable",
        ))
    }

    pub fn is_forgotten(&self, id: &str) -> Result<bool> {
        Ok(self.access.query_row(
            "SELECT EXISTS(SELECT 1 FROM forgotten_items WHERE item_id=?1)",
            [id],
            |r| r.get(0),
        )?)
    }

    pub fn forgotten_ids(&self) -> Result<BTreeSet<String>> {
        let mut stmt = self.access.prepare("SELECT item_id FROM forgotten_items")?;
        Ok(stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<_, _>>()?)
    }

    pub fn history(&self, id: &str) -> Result<Vec<(Member, String, Value)>> {
        let mut out = historical_members(&self.db, id)?;
        for db in archive_connections(&self.root, &self.generation)? {
            out.extend(historical_members(&db, id)?);
        }
        Ok(out
            .into_iter()
            .map(|h| (h.member, h.view_id, h.meta))
            .collect())
    }

    /// Newest-first evidence published no later than a retained view. Scope
    /// filtering remains the caller's responsibility, as with `history`.
    pub fn history_at(&self, id: &str, view_id: &str) -> Result<Vec<(Member, String, Value)>> {
        let (bound_sequence, bound_row, bound_generation) = self.with_view_db(view_id, |db| {
            let (sequence, row) = db.query_row(
                &format!(
                    "SELECT {},rowid FROM views WHERE view_id=?1",
                    publication_column(db)?
                ),
                [view_id],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)),
            )?;
            Ok((sequence, row, db.path().expect("on-disk store").to_owned()))
        })?;
        let archives = archive_connections(&self.root, &self.generation)?;
        let mut out = historical_members(&self.db, id)?;
        for db in &archives {
            out.extend(historical_members(db, id)?);
        }
        let legacy_generations: BTreeSet<_> = out
            .iter()
            .filter(|h| h.publication_seq.is_none())
            .map(|h| h.generation.clone())
            .chain(bound_sequence.is_none().then_some(bound_generation.clone()))
            .collect();
        let predecessors = if legacy_generations.len() > 1 {
            let legacy_views = out
                .iter()
                .filter(|h| h.publication_seq.is_none())
                .map(|h| h.view_id.clone())
                .chain(bound_sequence.is_none().then_some(view_id.to_owned()))
                .collect();
            legacy_view_predecessors(std::iter::once(&self.db).chain(&archives), &legacy_views)?
        } else {
            BTreeMap::new()
        };
        let earlier =
            |a: &str, b: &str| predecessors.get(b).is_some_and(|before| before.contains(a));
        let mut bounded = Vec::new();
        for h in out {
            let eligible = match (h.publication_seq, bound_sequence) {
                (Some(sequence), Some(bound)) => sequence <= bound,
                // Every legacy view predates the introduction of the clock.
                (None, Some(_)) => true,
                (Some(_), None) => false,
                (None, None) if h.generation == bound_generation => h.row <= bound_row,
                (None, None) if earlier(&h.view_id, view_id) => true,
                (None, None) if earlier(view_id, &h.view_id) => false,
                _ => return Err(history_order_unavailable()),
            };
            if eligible {
                bounded.push(h);
            }
        }
        let legacy_views: Vec<_> = bounded
            .iter()
            .filter(|h| h.publication_seq.is_none())
            .map(|h| (&h.view_id, h))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .collect();
        for (index, a) in legacy_views.iter().enumerate() {
            for b in &legacy_views[index + 1..] {
                if a.generation != b.generation
                    && !earlier(&a.view_id, &b.view_id)
                    && !earlier(&b.view_id, &a.view_id)
                {
                    return Err(history_order_unavailable());
                }
            }
        }
        bounded.sort_by(|a, b| {
            b.publication_seq
                .cmp(&a.publication_seq)
                .then_with(|| {
                    let depth = |view: &str| predecessors.get(view).map_or(0, BTreeSet::len);
                    depth(&b.view_id).cmp(&depth(&a.view_id))
                })
                .then_with(|| b.row.cmp(&a.row))
                .then_with(|| a.member.rank.cmp(&b.member.rank))
        });
        Ok(bounded
            .into_iter()
            .map(|h| (h.member, h.view_id, h.meta))
            .collect())
    }

    pub fn sources(&self, worktree: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .db
            .prepare("SELECT source_id FROM sources WHERE worktree_key=?1")?;
        let rows = stmt.query_map([worktree], |r| r.get(0))?;
        let mut known: std::collections::BTreeSet<String> =
            rows.collect::<std::result::Result<_, _>>()?;
        let mirror: Option<String> = self
            .access
            .query_row(
                "SELECT value FROM identity WHERE key=?1",
                [format!("source_ids:{}", util::hash(worktree))],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(mirror) = mirror {
            known.extend(serde_json::from_str::<Vec<String>>(&mirror)?);
        }
        Ok(known.into_iter().collect())
    }

    /// Advisory capture input from one committed index snapshot. Capture owns
    /// cursor interpretation; storage loads only the exact referenced versions.
    pub fn capture_cache(&self) -> Result<crate::capture::CaptureCacheInput> {
        let forgotten_ids = self.forgotten_ids()?;
        let tx = self.db.unchecked_transaction()?;
        let mut cache = crate::capture::CaptureCacheInput {
            forgotten_ids,
            ..crate::capture::CaptureCacheInput::default()
        };
        {
            let mut stmt = tx.prepare("SELECT source_id,source_path,json FROM cursors")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })? {
                let (source, path, cursor) = row?;
                // A malformed advisory cursor is a cache miss. Unreadable
                // retained payloads below must still return an error.
                if let Ok(cursor) = serde_json::from_str::<Value>(&cursor) {
                    cache.cursors.insert((source, path), cursor);
                }
            }
        }
        let referenced = cache.referenced_versions();
        if !referenced.is_empty() {
            let mut stmt = tx.prepare(
                "SELECT version_id,payload_json FROM item_versions
                 WHERE version_id IN (SELECT value FROM json_each(?1))
                 AND item_id NOT IN (SELECT value FROM json_each(?2))",
            )?;
            for row in stmt.query_map(
                params![
                    serde_json::to_string(&referenced)?,
                    serde_json::to_string(&cache.forgotten_ids)?,
                ],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )? {
                let (version, payload) = row?;
                cache
                    .payloads_by_version
                    .insert(version, serde_json::from_str(&payload)?);
            }
        }
        tx.commit()?;
        Ok(cache)
    }

    pub fn frecency_hash(&self) -> Result<String> {
        let mut stmt = self
            .access
            .prepare("SELECT item_id,count,last_at FROM frecency ORDER BY item_id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        util::hash_json(&rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn note_source_seen(
        &self,
        worktree: &str,
        source: &str,
        branch: &str,
        origin: &str,
    ) -> Result<bool> {
        let key = note_seen_key(worktree, source, branch, origin);
        let remembered: bool = self.access.query_row(
            "SELECT EXISTS(SELECT 1 FROM identity WHERE key=?1)",
            [&key],
            |r| r.get(0),
        )?;
        if remembered {
            return Ok(true);
        }
        let contains = |db: &Connection| -> Result<bool> {
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM view_items vi JOIN views v ON v.view_id=vi.view_id
                 WHERE json_extract(v.meta_json,'$.scope.worktree')=?1
                 AND json_extract(vi.member_json,'$.source_id')=?2
                 AND json_extract(vi.member_json,'$.kind')='memq-notes'
                 AND vi.branch=?3 AND vi.origin=?4)",
                params![worktree, source, branch, origin],
                |r| r.get(0),
            )?)
        };
        if contains(&self.db)? {
            return Ok(true);
        }
        if let Some(previous) = &self.previous {
            return contains(previous);
        }
        Ok(false)
    }

    pub fn touch(&self, ids: &[String]) -> Result<()> {
        let tx = self.access.unchecked_transaction()?;
        for id in ids {
            tx.execute(
                "INSERT INTO frecency VALUES(?1,1,?2) ON CONFLICT(item_id) DO UPDATE SET count=count+1,last_at=excluded.last_at",
                params![id,util::now()]
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn publish(
        &mut self,
        view: &View,
        items: &[Item],
        sources: &[(String, Value)],
        cursors: &[(String, String, Value)],
        validate_sources: impl FnOnce() -> Result<bool>,
    ) -> Result<()> {
        let forgotten = self.forgotten_ids()?;
        if items.iter().any(|item| forgotten.contains(&item.id))
            || view
                .members
                .iter()
                .any(|member| forgotten.contains(&member.id))
        {
            return Err(Error::new(
                "forgotten_identity",
                "publication contains a persistently forgotten identity",
            ));
        }
        // The mutation lock serializes allocations across every generation.
        // Allocate before committing the index: a failed publication may leave
        // a gap, but cannot make a later view appear older.
        let publication_seq: i64 = self.access.query_row(
            "UPDATE publication_clock SET sequence=sequence+1 WHERE singleton=1 RETURNING sequence",
            [],
            |r| r.get(0),
        )?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for item in items {
            tx.execute(
                "INSERT OR IGNORE INTO items(item_id,source_id,native_id) VALUES(?1,?2,?3)",
                params![item.id, item.source_id, item.payload.native_id],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO item_versions(version_id,item_id,content_sha256,payload_json,redacted_text,normalized_text)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![item.version_id,item.id,item.content_sha256,serde_json::to_string(&item.payload)?,item.payload.redacted_text,crate::records::normalized(&item.payload.redacted_text)]
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO embed_queue VALUES(?1)",
                [&item.version_id],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO observations(observation_id,version_id,worktree_key,origin,json) VALUES(?1,?2,?3,?4,?5)",
                params![item.observation.observation_id,item.version_id,item.observation.worktree_key,item.observation.origin,serde_json::to_string(&item.observation)?]
            )?;
        }
        for member in &view.members {
            tx.execute(
                "INSERT OR IGNORE INTO observations(observation_id,version_id,worktree_key,origin,json) VALUES(?1,?2,?3,?4,?5)",
                params![member.observation.observation_id,member.version_id,member.observation.worktree_key,member.observation.origin,serde_json::to_string(&member.observation)?]
            )?;
        }
        util::fault("mid_capture_batch");
        let worktree = view.meta["scope"]["worktree"]
            .as_str()
            .expect("scope worktree");
        tx.execute("DELETE FROM sources WHERE worktree_key=?1", [worktree])?;
        for (source_id, config) in sources {
            tx.execute(
                "INSERT INTO sources VALUES(?1,?2,?3)",
                params![worktree, source_id, serde_json::to_string(config)?],
            )?;
        }
        tx.execute(
            "UPDATE views SET is_current=0 WHERE scope_hash=?1",
            [&view.scope_hash],
        )?;
        tx.execute(
            "INSERT INTO views(view_id,scope_hash,inputs_hash,meta_json,is_current,publication_seq)
             VALUES(?1,?2,?3,?4,1,?5)",
            params![
                view.id,
                view.scope_hash,
                view.inputs_hash,
                serde_json::to_string(&view.meta)?,
                publication_seq,
            ],
        )?;
        for m in &view.members {
            tx.execute(
                "INSERT INTO view_items VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    view.id,
                    util::id(),
                    m.id,
                    m.version_id,
                    m.observation.observation_id,
                    m.observation.origin,
                    m.observation.branch,
                    serde_json::to_string(m)?
                ],
            )?;
            for relation in &m.relations {
                tx.execute(
                    "INSERT OR REPLACE INTO relations VALUES(?1,?2,?3,?4)",
                    params![
                        m.version_id,
                        relation["field"].as_str().unwrap_or(""),
                        relation["to"].as_str().unwrap_or(""),
                        serde_json::to_string(relation)?
                    ],
                )?;
            }
        }
        for (source, path, cursor) in cursors {
            tx.execute(
                "INSERT OR REPLACE INTO cursors VALUES(?1,?2,?3)",
                params![source, path, serde_json::to_string(cursor)?],
            )?;
        }
        util::fault("before_publish");
        if !validate_sources()? {
            return Err(Error::new(
                "source_race",
                "sources changed before publication commit",
            ));
        }
        tx.commit()?;
        self.access.execute(
            "INSERT OR REPLACE INTO identity VALUES(?1,?2)",
            params![
                format!("source_ids:{}", util::hash(worktree)),
                serde_json::to_string(&sources.iter().map(|(id, _)| id).collect::<Vec<_>>())?
            ],
        )?;
        for member in &view.members {
            if member.kind == "memq-notes" {
                self.access.execute(
                    "INSERT OR IGNORE INTO identity VALUES(?1,'1')",
                    [note_seen_key(
                        worktree,
                        &member.source_id,
                        &member.observation.branch,
                        &member.observation.origin,
                    )],
                )?;
            }
        }
        if !items.is_empty() {
            self.access.execute("INSERT INTO source_inventory VALUES(?1,1) ON CONFLICT(worktree) DO UPDATE SET had_items=1",[worktree])?;
        }
        if self.replacement {
            self.validate()?;
            self.db.execute_batch("PRAGMA wal_checkpoint(FULL);")?;
            util::atomic_replace(
                &self
                    .root
                    .join("generations")
                    .join(&self.generation)
                    .join("VERIFIED"),
                b"1",
            )?;
            util::atomic_replace(&self.root.join("CURRENT"), self.generation.as_bytes())?;
            self.replacement = false;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        compatible(&self.db)?;
        if !healthy(&self.db)? {
            return Err(Error::new(
                "repair_failed",
                "replacement quick_check failed",
            ));
        }
        self.db.execute_batch(
            "INSERT INTO items_fts(items_fts,rank) VALUES('integrity-check',1);
             INSERT INTO items_trigram(items_trigram,rank) VALUES('integrity-check',1);",
        )?;
        Ok(())
    }
}

fn note_seen_key(worktree: &str, source: &str, branch: &str, origin: &str) -> String {
    format!(
        "note_source_seen:{}",
        util::hash(format!("{worktree}\n{source}\n{branch}\n{origin}"))
    )
}

fn history_order_unavailable() -> Error {
    Error::new(
        "history_order_unavailable",
        "retained legacy views have no established publication order",
    )
}

// Before v3, SQLite insertion order establishes chronology within a generation.
// Cross-generation baselines establish order only between the named views:
// an interrupted CURRENT switch may leave the old generation writable. Missing
// links do not justify guessing by IDs, directory names, or wall-clock strings.
fn legacy_view_predecessors<'a>(
    databases: impl Iterator<Item = &'a Connection>,
    requested: &BTreeSet<String>,
) -> Result<BTreeMap<String, BTreeSet<String>>> {
    let mut links = BTreeMap::<String, BTreeSet<String>>::new();
    for db in databases {
        let mut previous = None;
        let mut stmt = db.prepare("SELECT view_id,meta_json FROM views ORDER BY rowid")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, meta) = row?;
            let meta: Value = serde_json::from_str(&meta)?;
            let parents = links.entry(id.clone()).or_default();
            if let Some(baseline) = meta["changes_since"]["baseline"].as_str() {
                parents.insert(baseline.to_owned());
            }
            if let Some(before) = previous {
                parents.insert(before);
            }
            previous = Some(id);
        }
    }
    let mut predecessors = BTreeMap::new();
    for id in requested {
        let mut ancestors = BTreeSet::new();
        let mut active = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut pending = vec![(id.clone(), false)];
        while let Some((view, finished)) = pending.pop() {
            if finished {
                active.remove(&view);
                visited.insert(view);
                continue;
            }
            if active.contains(&view) {
                return Err(history_order_unavailable());
            }
            if visited.contains(&view) {
                continue;
            }
            active.insert(view.clone());
            pending.push((view.clone(), true));
            if let Some(parents) = links.get(&view) {
                for parent in parents {
                    ancestors.insert(parent.clone());
                    pending.push((parent.clone(), false));
                }
            }
        }
        predecessors.insert(id.clone(), ancestors);
    }
    Ok(predecessors)
}

fn archive_connections(root: &Path, current: &str) -> Result<Vec<Connection>> {
    let selected = fs::read_to_string(root.join("CURRENT")).unwrap_or_default();
    let mut directories =
        fs::read_dir(root.join("generations"))?.collect::<std::io::Result<Vec<_>>>()?;
    directories.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    let mut connections = Vec::new();
    for directory in directories {
        let name = directory.file_name().to_string_lossy().into_owned();
        if name == current
            || (!directory.path().join("VERIFIED").exists() && name != selected.trim())
        {
            continue;
        }
        let path = directory.path().join("index.sqlite");
        if !path.exists() {
            continue;
        }
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        match compatible(&db).and_then(|()| healthy(&db)) {
            Ok(true) if history_layout(&db) => connections.push(db),
            Ok(true) => (),
            Ok(false) => (),
            Err(e) if corrupt_error(&e) => (),
            Err(e) => return Err(e),
        }
    }
    Ok(connections)
}

pub fn load_view(db: &Connection, id: &str) -> Result<View> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT scope_hash,inputs_hash,meta_json FROM views WHERE view_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let (scope_hash, inputs_hash, meta) =
        row.ok_or_else(|| Error::new("stale_continuation", "view no longer available"))?;
    let mut stmt = db.prepare("SELECT member_json FROM view_items WHERE view_id=?1")?;
    let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
    let mut members: Vec<Member> = Vec::new();
    for row in rows {
        members.push(serde_json::from_str(&row?)?);
    }
    // Rank is assigned to the canonical occurrence vector before publication.
    // member_seq identifies a row; random ULIDs do not encode insertion order.
    members.sort_by_key(|m| m.rank);
    Ok(View {
        id: id.into(),
        scope_hash,
        inputs_hash,
        meta: serde_json::from_str(&meta)?,
        members,
    })
}
