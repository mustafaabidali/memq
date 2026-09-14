//! Shared command core. It owns the writer lock and orders refresh, repair,
//! evidence reads, durable writes, and embedding publication for all transports.
use crate::config::Config;
use crate::error::{Error, Result};
use crate::notes;
use crate::repository::Repository;
use crate::store::{MutationLock, Store, data_root};
use crate::tombstone::{self, Tombstone};
use crate::util;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadRequest {
    pub task: Option<String>,
    pub branches: Vec<String>,
    pub incoming: Option<bool>,
    pub budget: Option<usize>,
    pub budget_kind: Option<String>,
    pub tokenizer: Option<String>,
    pub compact: bool,
    pub query: Option<String>,
    pub ids: Vec<String>,
    pub view_id: Option<String>,
    pub continuation: Option<String>,
    pub allow_source_removal: bool,
}

pub struct Service {
    pub(crate) repo: Repository,
    pub(crate) config: Config,
    pub(crate) store: Store,
    pub(crate) tombstones: Vec<Tombstone>,
    _lock: MutationLock,
}

impl Service {
    pub fn open(path: &Path, rebuild: bool) -> Result<Self> {
        let repo = Repository::discover(path)?;
        let config = Config::load(&repo.root)?;
        let root = data_root(&repo.clone_id)?;
        let lock = MutationLock::acquire(&root)?;
        let store = Store::open(&root, &repo.clone_id, rebuild)?;
        let remembered: Option<String> = store
            .access
            .query_row(
                "SELECT value FROM identity WHERE key='project_id'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if remembered.as_ref().is_some_and(|p| p != &config.project_id) {
            return Err(Error::new(
                "invalid_config",
                "project_id changed in this clone",
            ));
        }
        let project: Option<String> = store
            .db
            .query_row("SELECT value FROM meta WHERE key='project_id'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if project.as_ref().is_some_and(|s| s != &config.project_id) {
            return Err(Error::new(
                "invalid_config",
                "project_id changed in an existing clone index",
            ));
        }
        store.access.execute(
            "INSERT OR IGNORE INTO identity VALUES('project_id',?1)",
            [&config.project_id],
        )?;
        store.db.execute(
            "INSERT OR IGNORE INTO meta VALUES('project_id',?1)",
            [&config.project_id],
        )?;
        let tombstones = tombstone::load(&repo, &store.access)?;
        tombstone::apply(&store, &tombstones)?;
        Ok(Self {
            repo,
            config,
            store,
            tombstones,
            _lock: lock,
        })
    }

    pub fn initialize(path: &Path) -> Result<Value> {
        let repo = Repository::discover(path)?;
        let config = Config::init(&repo.root)?;
        let view = Self::open(path, false)?.reconcile(&ReadRequest::default(), "reconcile")?;
        Ok(
            json!({"project_id":config.project_id,"config":".memq/config.toml",
            "view_id":view.id,"parameters":"provisional"}),
        )
    }

    pub fn project_id(&self) -> &str {
        &self.config.project_id
    }

    pub fn doctor(&self, gc: bool, probe_harnesses: bool) -> Result<Value> {
        self.store.validate()?;
        if gc {
            tombstone::apply(&self.store, &self.tombstones)?;
        }
        Ok(json!({"status":"ok","sqlite":self.store.sqlite_info()?,
            "generation":self.store.generation,"historical_evidence":"retained",
            "code":crate::code_evidence::load(&self.repo,&self.config.project_id,&self.repo.state()?).coverage,
            "harnesses":if probe_harnesses { crate::capture::probe_defaults() } else { Value::Null }}))
    }

    pub fn note(&mut self, request: notes::NoteRequest) -> Result<Value> {
        let recovery = match self.reconcile(&ReadRequest::default(), "note") {
            Ok(view) if view.meta["freshness"]["reason"] == "recovery_limit_reached" => Some(
                Error::new(
                    "recovery_limit_reached",
                    json!({
                        "coverage":view.meta["coverage"],
                        "reason":"supporting sources are unavailable; restore their context"
                    }),
                )
                .detail,
            ),
            Ok(_) => None,
            Err(error) if error.code == "recovery_limit_reached" => Some(error.detail),
            Err(error) => return Err(error),
        };
        let mut result = notes::write(
            &self.repo,
            &self.config,
            &self.store,
            &self.tombstones,
            request,
        )?;
        if result["durability"] == "durable"
            && let Some(detail) = recovery
        {
            // Saving explicit new context does not restore missing history.
            // Forgotten retry placeholders retain their content-free shape.
            result["recovery"] = json!({"code":"recovery_limit_reached","detail":detail});
        }
        Ok(result)
    }

    pub fn forget(&mut self, predicate: tombstone::Predicate) -> Result<Value> {
        if predicate
            .project_id
            .as_ref()
            .is_some_and(|p| p != &self.config.project_id)
            || predicate
                .item_id
                .as_ref()
                .is_some_and(|p| !p.starts_with(&format!("mq:{}:", self.config.project_id)))
        {
            return Err(Error::new(
                "invalid_tombstone",
                "forget scope belongs to another project",
            ));
        }
        let t = Tombstone {
            format: 1,
            identity_hash: predicate.hash()?,
            scope: predicate,
            forgotten_at: util::now(),
            purge_epoch: util::id(),
        };
        t.validate()?;
        let directory = self.repo.root.join(".memq/tombstones");
        std::fs::create_dir_all(&directory)?;
        if !std::fs::canonicalize(&directory)?.starts_with(&self.repo.root) {
            return Err(Error::new(
                "invalid_tombstone",
                "tombstone directory escapes worktree",
            ));
        }
        let filename = format!("{}-{}.json", t.identity_hash, t.purge_epoch);
        let temp = directory.join(format!(".{filename}.tmp"));
        util::write_private(&temp, &util::canonical(&t)?)?;
        util::publish_new(&temp, &directory.join(&filename))?;
        self.store.access.execute(
            "INSERT OR REPLACE INTO tombstones VALUES(?1,?2)",
            params![t.identity_hash, serde_json::to_string(&t)?],
        )?;
        self.tombstones = tombstone::load(&self.repo, &self.store.access)?;
        tombstone::apply(&self.store, &self.tombstones)?;
        let staging = notes::stage(&self.repo, &format!(".memq/tombstones/{filename}"))?;
        Ok(
            json!({"forgotten":t.scope,"purge":"complete","staging":staging["status"],"durability":"durable","tombstone":filename}),
        )
    }
}
