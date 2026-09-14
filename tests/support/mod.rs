#![allow(dead_code)]
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

pub struct Repo {
    pub temp: TempDir,
    pub root: PathBuf,
    pub data: PathBuf,
}

impl Repo {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        let data = temp.path().join("data");
        fs::create_dir(&root).unwrap();
        let repo = Self { temp, root, data };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.write("seed.txt", "Synthetic project\n");
        repo.commit("fixture seed");
        repo
    }
    pub fn initialized() -> Self {
        let r = Self::new();
        r.ok(&["init"]);
        r
    }
    pub fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_memq"));
        cmd.arg("--repo")
            .arg(&self.root)
            .env("MEMQ_DATA_DIR", &self.data)
            .env("MEMQ_NOW", "2026-09-13T20:00:00Z");
        cmd
    }
    pub fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    pub fn ok(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "memq {:?}: status {:?}\n{}\n{}",
            args,
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    pub fn error(&self, args: &[&str], code: &str) -> Value {
        let out = self.run(args);
        assert!(
            !out.status.success(),
            "unexpected success: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["error"]["code"], code, "{v}");
        v
    }
    pub fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }
    pub fn write(&self, path: &str, text: impl AsRef<[u8]>) {
        let target = self.root.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
    }
    pub fn commit(&self, message: &str) {
        self.git(&["add", "--all"]);
        self.git(&["commit", "-q", "--no-gpg-sign", "-m", message]);
    }
    pub fn add_source(&self, source: &str) {
        use std::io::Write;
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(self.root.join(".memq/config.toml"))
            .unwrap();
        writeln!(f, "\n{source}").unwrap();
    }
    pub fn records(&self, records: Value) {
        self.write(
            "records.json",
            serde_json::to_vec(&serde_json::json!({"records":records})).unwrap(),
        );
    }
    pub fn record_source(&self) {
        self.add_source(
            r#"[[source]]
id = "records"
kind = "json-records"
path = "records.json"
collection = "records"
id_field = "id"
references = ["file", "depends_on"]
[source.policy]
status_field = "status"
accepted_values = ["accepted"]
proposed_values = ["proposed"]
withdrawn_values = ["withdrawn"]
blocked_values = ["blocked"]
active_values = ["in_progress", "pending"]
supersedes_field = "supersedes"
reason_field = "reason"
decider_field = "decider"
approvers = ["owner", "maintainer"]
require_attribution = true
"#,
        );
    }
    pub fn db_path(&self) -> PathBuf {
        let repo = memq::repository::Repository::discover(&self.root).unwrap();
        let store = self.data.join(repo.clone_id);
        let generation = fs::read_to_string(store.join("CURRENT")).unwrap();
        store
            .join("generations")
            .join(generation)
            .join("index.sqlite")
    }
    pub fn store_root(&self) -> PathBuf {
        self.db_path()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned()
    }
    pub fn linked(&self, branch: &str) -> PathBuf {
        let path = self.temp.path().join(branch);
        self.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            branch,
            path.to_str().unwrap(),
        ]);
        path
    }
    pub fn clone_repo(&self) -> PathBuf {
        let path = self.temp.path().join("clone");
        git(
            self.temp.path(),
            &[
                "clone",
                "-q",
                self.root.to_str().unwrap(),
                path.to_str().unwrap(),
            ],
        );
        path
    }
    pub fn bare_remote(&self) -> PathBuf {
        let path = self.temp.path().join("remote.git");
        git(
            self.temp.path(),
            &["init", "--bare", "-q", path.to_str().unwrap()],
        );
        path
    }
}

pub fn git(path: &Path, args: &[&str]) -> String {
    // Identity applies only to commands in synthetic temporary repositories.
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-09-13T18:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-09-13T18:00:00Z")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

pub fn budget_check(raw: &[u8]) {
    let value: Value = serde_json::from_slice(raw).unwrap();
    // This helper independently counts the actual wire payload, including every field.
    let text = std::str::from_utf8(raw).unwrap().trim_end_matches('\n');
    let count = if value["budget"]["kind"] == "bytes" {
        text.len()
    } else {
        let bpe = if value["budget"]["encoding"] == "cl100k_base" {
            tiktoken_rs::cl100k_base().unwrap()
        } else {
            tiktoken_rs::o200k_base().unwrap()
        };
        bpe.encode_ordinary(text).len()
    };
    assert_eq!(
        value["budget"]["used"].as_u64().unwrap() as usize,
        count,
        "actual response count differs"
    );
    assert!(
        count <= value["budget"]["limit"].as_u64().unwrap() as usize,
        "actual wire payload exceeds limit"
    );
}

/// Independently expand the optional shared wire encoding. Budget checks must
/// use the original bytes, before expanding this response-local dictionary.
pub fn expanded_items(response: &Value) -> Vec<Value> {
    if response["memq"]["encoding"] != "shared-v1" {
        return response["items"].as_array().unwrap().clone();
    }
    response["occurrences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|occurrence| {
            let body = &response["items"][occurrence[0].as_u64().unwrap() as usize];
            let Some(source_index) = occurrence[1].as_u64() else {
                return body.clone();
            };
            let source = &response["sources"][source_index as usize];
            let mut result = response["item_defaults"].as_object().unwrap().clone();
            result.extend(body.as_object().unwrap().clone());
            result.insert("observation".into(), source["observation"].clone());
            if let Some(availability) = source.get("availability") {
                result.insert("availability".into(), availability.clone());
            }
            result.insert(
                "pointer".into(),
                Value::String(format!(
                    "{}{}",
                    source["pointer_prefix"].as_str().unwrap(),
                    body["pointer"].as_str().unwrap()
                )),
            );
            Value::Object(result)
        })
        .collect()
}
