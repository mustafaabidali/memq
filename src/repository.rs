use crate::error::{Error, Result};
use crate::util;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

/// The caller resolves any permitted symlink and checks its scope first.
/// Check before opening to avoid device access, then check the open descriptor
/// before reading. O_NONBLOCK also prevents a FIFO replacement from hanging
/// between those checks; O_NOFOLLOW refuses a replacement symlink.
pub(crate) fn read_regular_file(path: &Path) -> Result<Vec<u8>> {
    let nonregular = || Error::new("source_not_regular", "refusing to read a nonregular file");
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(nonregular());
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(nonregular());
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone, Debug)]
pub struct Repository {
    pub root: PathBuf,
    pub common: PathBuf,
    pub clone_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GitState {
    pub head: Option<String>,
    pub object_format: String,
    pub branch: String,
    pub dirty: BTreeMap<String, String>,
}

/// A revision-scoped tree and one streaming blob reader. Tree membership alone
/// is insufficient: the blob must be readable and contain the observed bytes.
pub struct BlobWitness {
    blobs: BTreeMap<String, String>,
    batch: Option<BlobBatch>,
}

impl BlobWitness {
    pub fn matching_oid(&mut self, path: &str, bytes: &[u8]) -> Result<Option<String>> {
        let Some(oid) = self.blobs.get(path) else {
            return Ok(None);
        };
        let Some(batch) = self.batch.as_mut() else {
            return Ok(None);
        };
        match batch.matches(oid, bytes) {
            Ok(matches) => Ok(matches.then(|| oid.clone())),
            Err(error) => {
                // A damaged stream cannot attest another file. Do not restart
                // one failing Git process per remaining source.
                self.batch = None;
                Err(error)
            }
        }
    }
}

struct BlobBatch {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl BlobBatch {
    fn matches(&mut self, oid: &str, bytes: &[u8]) -> Result<bool> {
        let input = self.input.as_mut().expect("open batch input");
        writeln!(input, "{oid}")?;
        input.flush()?;
        let mut header = String::new();
        self.output.read_line(&mut header)?;
        let fields: Vec<_> = header.split_whitespace().collect();
        if fields == [oid, "missing"] {
            return Ok(false);
        }
        if fields.len() != 3 || fields[0] != oid || fields[1] != "blob" {
            return Err(Error::new("git_error", "invalid Git blob batch response"));
        }
        let mut remaining: u64 = fields[2]
            .parse()
            .map_err(|_| Error::new("git_error", "invalid Git blob size"))?;
        let mut equal = remaining == bytes.len() as u64;
        let mut offset = 0usize;
        let mut buffer = [0u8; 64 * 1024];
        while remaining != 0 {
            let count = remaining.min(buffer.len() as u64) as usize;
            self.output.read_exact(&mut buffer[..count])?;
            if equal && bytes[offset..offset + count] != buffer[..count] {
                equal = false;
            }
            offset = offset.saturating_add(count);
            remaining -= count as u64;
        }
        let mut terminator = [0u8; 1];
        self.output.read_exact(&mut terminator)?;
        if terminator != *b"\n" {
            return Err(Error::new("git_error", "invalid Git blob batch delimiter"));
        }
        Ok(equal)
    }
}

impl Drop for BlobBatch {
    fn drop(&mut self) {
        self.input.take();
        // Also handles a malformed/truncated response with unread output.
        // Waiting without terminating that writer could block on its pipe.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Repository {
    pub fn discover(path: &Path) -> Result<Self> {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["rev-parse", "--show-toplevel"])
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()?;
        if !output.status.success() {
            return Err(Error::new(
                "not_a_git_repository",
                "a Git worktree is required",
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|_| Error::new("git_error", "non-UTF-8 repository path"))?;
        let root = fs::canonicalize(path.strip_suffix('\n').unwrap_or(&path))?;
        let output = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()?;
        if !output.status.success() {
            return Err(Error::new(
                "not_a_git_repository",
                "cannot resolve Git common directory",
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|_| Error::new("git_error", "non-UTF-8 common directory"))?;
        let common = fs::canonicalize(path.strip_suffix('\n').unwrap_or(&path))?;
        let clone_id = util::hash(common.to_string_lossy().as_bytes());
        Ok(Self {
            root,
            common,
            clone_id,
        })
    }

    pub fn git(&self, args: &[&str]) -> Result<Output> {
        Ok(self.git_command().args(args).output()?)
    }

    fn git_command(&self) -> Command {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.root)
            .env("GIT_OPTIONAL_LOCKS", "0");
        command
    }

    pub fn worktrees(&self) -> Result<Vec<Self>> {
        let out = self.git(&["worktree", "list", "--porcelain", "-z"])?;
        if out.status.code() == Some(129) {
            return self.legacy_worktrees();
        }
        if !out.status.success() {
            return Err(Error::new("git_error", "cannot enumerate linked worktrees"));
        }
        let mut repos = Vec::new();
        for field in out.stdout.split(|b| *b == 0) {
            if let Some(path) = field.strip_prefix(b"worktree ") {
                let path = std::str::from_utf8(path)
                    .map_err(|_| Error::new("git_error", "non-UTF-8 worktree path"))?;
                if let Ok(repo) = Self::discover(Path::new(path))
                    && repo.common == self.common
                {
                    repos.push(repo);
                }
            }
        }
        Ok(repos)
    }

    fn legacy_worktrees(&self) -> Result<Vec<Self>> {
        // Git 2.34 rejects -z and emits raw paths in its line-based listing.
        // Use its per-worktree backlinks instead: each file holds one complete
        // path followed by one newline, so embedded newlines remain path data.
        let mut paths = vec![self.root.clone()];
        if self.common.file_name().is_some_and(|n| n == ".git") {
            paths.push(
                self.common
                    .parent()
                    .expect("absolute Git directory")
                    .to_owned(),
            );
        } else if self.text(&["config", "--bool", "--get", "core.bare"])? != "true" {
            return Err(Error::new(
                "git_error",
                "this Git version cannot safely list this separate Git directory; use a Git version with worktree list -z",
            ));
        }
        match fs::read_dir(self.common.join("worktrees")) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if !entry.path().is_dir() {
                        continue;
                    }
                    let bytes = read_regular_file(&entry.path().join("gitdir")).map_err(|_| {
                        Error::new(
                            "git_error",
                            "linked worktree Git backlink must be a readable regular file",
                        )
                    })?;
                    let value = String::from_utf8(bytes)
                        .map_err(|_| Error::new("git_error", "non-UTF-8 worktree Git backlink"))?;
                    let path = Path::new(value.strip_suffix('\n').unwrap_or(&value));
                    if path.file_name().is_none_or(|n| n != ".git") {
                        return Err(Error::new(
                            "git_error",
                            "invalid linked worktree Git backlink",
                        ));
                    }
                    let path = if path.is_absolute() {
                        path.to_owned()
                    } else {
                        entry.path().join(path)
                    };
                    paths.push(path.parent().expect("Git backlink parent").to_owned());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        paths.sort();
        paths.dedup();
        let mut repos = Vec::new();
        for path in paths {
            if !path.exists() {
                // Git retains entries for deleted, prunable worktrees.
                continue;
            }
            let repo = Self::discover(&path).map_err(|_| {
                Error::new(
                    "git_error",
                    "cannot resolve a registered worktree; check its Git metadata",
                )
            })?;
            if repo.common != self.common {
                return Err(Error::new(
                    "git_error",
                    "worktree backlink points to a different clone",
                ));
            }
            repos.push(repo);
        }
        Ok(repos)
    }

    pub fn text(&self, args: &[&str]) -> Result<String> {
        let output = self.git(args)?;
        if !output.status.success() {
            return Err(Error::new(
                "git_error",
                json!({"operation": args.first(), "exit": output.status.code()}),
            ));
        }
        String::from_utf8(output.stdout)
            .map(|s| s.strip_suffix('\n').unwrap_or(&s).to_owned())
            .map_err(|_| Error::new("git_error", "Git returned a non-UTF-8 path"))
    }

    pub fn state(&self) -> Result<GitState> {
        let object_format = self.text(&["rev-parse", "--show-object-format"])?;
        let head = self.git(&["rev-parse", "--verify", "HEAD"])?.stdout;
        let head = String::from_utf8_lossy(&head).trim().to_owned();
        let branch = self.git(&["symbolic-ref", "--quiet", "--short", "HEAD"])?;
        let branch = if branch.status.success() {
            String::from_utf8_lossy(&branch.stdout).trim().to_owned()
        } else {
            "HEAD".into()
        };
        let status = self.git(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        if !status.status.success() {
            return Err(Error::new("git_error", "cannot inspect working state"));
        }
        let mut dirty = BTreeMap::new();
        let mut entries = status.stdout.split(|b| *b == 0);
        while let Some(e) = entries.next() {
            if e.len() < 4 {
                continue;
            }
            let path = String::from_utf8(e[3..].to_vec())
                .map_err(|_| Error::new("git_error", "non-UTF-8 working path"))?;
            let state = String::from_utf8_lossy(&e[..2]).to_string();
            let rename = state.contains('R') || state.contains('C');
            dirty.insert(path, state);
            if rename && let Some(old) = entries.next() {
                let old = String::from_utf8(old.to_vec())
                    .map_err(|_| Error::new("git_error", "non-UTF-8 working path"))?;
                dirty.insert(old, "renamed_from".into());
            }
        }
        Ok(GitState {
            head: (!head.is_empty()).then_some(head),
            object_format,
            branch,
            dirty,
        })
    }

    pub fn resolve_branch(&self, branch: &str) -> Result<String> {
        let reference = format!("refs/heads/{branch}");
        if !self
            .git(&["check-ref-format", &reference])?
            .status
            .success()
        {
            return Err(Error::new("invalid_request", "invalid branch"));
        }
        self.text(&["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
            .map_err(|_| Error::new("invalid_request", json!({"missing_branch": branch})))
    }

    pub fn read_at(&self, path: &str, revision: Option<&str>) -> Result<Option<Vec<u8>>> {
        util::relative_path(path)?;
        if let Some(rev) = revision {
            let o = self.git(&["show", &format!("{rev}:{path}")])?;
            return Ok(o.status.success().then_some(o.stdout));
        }
        let full = self.root.join(path);
        match fs::canonicalize(&full) {
            Ok(real) => {
                if !real.starts_with(&self.root) {
                    return Err(Error::new(
                        "source_unreadable",
                        "source symlink leaves worktree",
                    ));
                }
                Ok(Some(read_regular_file(&real)?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn files_at(&self, dir: &str, revision: Option<&str>) -> Result<Vec<String>> {
        util::relative_path(dir)?;
        let mut files = Vec::new();
        if let Some(rev) = revision {
            let output = self.git(&["ls-tree", "-r", "-z", "--name-only", rev, "--", dir])?;
            if !output.status.success() {
                return Err(Error::new(
                    "git_error",
                    "cannot enumerate source at revision",
                ));
            }
            for p in output.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
                files.push(
                    String::from_utf8(p.to_vec())
                        .map_err(|_| Error::new("git_error", "non-UTF-8 source path"))?,
                );
            }
        } else if self.root.join(dir).exists() {
            for entry in walkdir::WalkDir::new(self.root.join(dir)).follow_links(false) {
                let entry = entry
                    .map_err(|_| Error::new("source_unreadable", "cannot enumerate source"))?;
                if entry.file_type().is_dir() {
                    continue;
                }
                let path = entry
                    .path()
                    .strip_prefix(&self.root)
                    .expect("walk inside root")
                    .to_string_lossy()
                    .into_owned();
                if !entry.file_type().is_file() {
                    return Err(Error::new(
                        "source_not_regular",
                        json!({"path":path,"reason":"source entries must be regular files or directories"}),
                    ));
                }
                files.push(path);
            }
        }
        files.sort();
        Ok(files)
    }

    pub fn blob_oid(&self, path: &str, revision: Option<&str>) -> Option<String> {
        let revision = revision?;
        self.text(&["rev-parse", "--verify", &format!("{revision}:{path}")])
            .ok()
    }

    /// Bound proof to the supplied revision and configured source roots, with
    /// at most two Git processes regardless of the number of source files.
    /// Literal, NUL-delimited tree paths preserve whitespace and pathspec bytes.
    pub fn blob_witness(&self, revision: &str, paths: &[&str]) -> Result<BlobWitness> {
        for path in paths {
            util::relative_path(path)?;
        }
        if paths.is_empty() {
            return Ok(BlobWitness {
                blobs: BTreeMap::new(),
                batch: None,
            });
        }
        let output = self
            .git_command()
            .env("GIT_NO_LAZY_FETCH", "1")
            .args([
                "--literal-pathspecs",
                "ls-tree",
                "-r",
                "-z",
                "--full-tree",
                revision,
                "--",
            ])
            .args(paths)
            .output()?;
        if !output.status.success() {
            return Err(Error::new("git_error", "cannot enumerate source blobs"));
        }
        let mut blobs = BTreeMap::new();
        for entry in output.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                return Err(Error::new("git_error", "invalid Git tree entry"));
            };
            let header = std::str::from_utf8(&entry[..tab])
                .map_err(|_| Error::new("git_error", "invalid Git tree header"))?;
            let fields: Vec<_> = header.split_whitespace().collect();
            if fields.len() != 3 {
                return Err(Error::new("git_error", "invalid Git tree header"));
            }
            if fields[1] != "blob" {
                continue;
            }
            let oid = fields[2];
            if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::new("git_error", "invalid Git blob identity"));
            }
            // Such a path cannot match a caller's UTF-8 source path.
            if let Ok(path) = std::str::from_utf8(&entry[tab + 1..]) {
                blobs.insert(path.to_owned(), oid.to_owned());
            }
        }
        let batch = if blobs.is_empty() {
            None
        } else {
            let mut child = self
                .git_command()
                .env("GIT_NO_LAZY_FETCH", "1")
                .args(["cat-file", "--batch"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?;
            Some(BlobBatch {
                input: child.stdin.take(),
                output: BufReader::new(child.stdout.take().expect("piped batch output")),
                child,
            })
        };
        Ok(BlobWitness { blobs, batch })
    }

    pub fn path_fingerprint(&self, path: &str, revision: Option<&str>) -> Value {
        match self.read_at(path, revision) {
            Ok(Some(b)) => json!([path, "present", util::hash(b)]),
            Ok(None) => json!([path, "missing", null]),
            Err(e) if e.code == "source_not_regular" => {
                eprintln!("memq: {e}; path={}", json!(crate::redact::text(path)));
                json!([path, "nonregular", null])
            }
            Err(_) => json!([path, "unreadable", null]),
        }
    }

    pub fn is_ancestor(&self, before: &str, after: &str) -> bool {
        self.git(&["merge-base", "--is-ancestor", before, after])
            .is_ok_and(|o| o.status.success())
    }

    pub fn changed_files(&self, before: &str, after: &str) -> Result<Vec<String>> {
        let output = self.git(&["diff", "--name-only", "-z", before, after, "--"])?;
        if !output.status.success() {
            return Ok(vec![]);
        }
        Ok(output
            .stdout
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    }
}
