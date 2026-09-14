mod support;
use support::{Repo, git};

#[test]
fn real_git_topologies() {
    let r = Repo::new();
    let root = memq::repository::Repository::discover(&r.root).unwrap();
    let linked = r.linked("side");
    assert_eq!(
        root.clone_id,
        memq::repository::Repository::discover(&linked)
            .unwrap()
            .clone_id
    );
    let clone = r.clone_repo();
    assert_ne!(
        root.clone_id,
        memq::repository::Repository::discover(&clone)
            .unwrap()
            .clone_id
    );
    let nested = r.root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q"]);
    assert_ne!(
        root.clone_id,
        memq::repository::Repository::discover(&nested)
            .unwrap()
            .clone_id
    );
    assert_eq!(root.state().unwrap().object_format, "sha1");
    let remote = r.bare_remote();
    assert_eq!(git(&remote, &["rev-parse", "--is-bare-repository"]), "true");
}

#[test]
fn controlled_fts_unavailable_is_an_error() {
    let r = Repo::new();
    let out = r
        .command()
        .env("MEMQ_FAULT", "unsupported_fts")
        .arg("init")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["error"]["code"], "unsupported_sqlite");
}
