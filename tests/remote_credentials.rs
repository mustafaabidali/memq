mod support;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use support::Repo;

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn bounded_remote_fetch_uses_configured_credential_helper() {
    let r = Repo::initialized();
    r.record_source();
    r.records(serde_json::json!([{"id":"known","text":"Credential fixture"}]));
    r.commit("fixture sources");
    let remote = r.bare_remote();
    r.git(&[
        "push",
        "-q",
        remote.to_str().unwrap(),
        "main:refs/heads/main",
    ]);
    let credential = memq::util::id();
    let mut server = Server(
        Command::new("python3")
            .arg(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/git_http.py"),
            )
            .arg(r.temp.path())
            .env("MEMQ_FIXTURE_CREDENTIAL", &credential)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut port = String::new();
    BufReader::new(server.0.stdout.take().unwrap())
        .read_line(&mut port)
        .unwrap();
    let port: u16 = port
        .trim()
        .parse()
        .expect("loopback Git fixture must start; this test cannot be skipped");
    let helper = r.temp.path().join("credential-helper");
    std::fs::write(
        &helper,
        r#"#!/usr/bin/env python3
import os,sys
from pathlib import Path
sys.stdin.read()
if sys.argv[-1] == "get":
    Path(os.environ["MEMQ_FIXTURE_HELPER_MARKER"]).write_text("used")
    print("username=fixture")
    print("password=" + os.environ["MEMQ_FIXTURE_CREDENTIAL"])
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    r.git(&["config", "--add", "credential.helper", ""]);
    r.git(&[
        "config",
        "--add",
        "credential.helper",
        &format!("!'{}'", helper.to_str().unwrap().replace('\'', "'\\''")),
    ]);
    r.git(&[
        "remote",
        "add",
        "origin",
        &format!("http://127.0.0.1:{port}/remote.git"),
    ]);
    r.add_source("[remote]\nname='origin'\nref='refs/heads/main'\ntimeout_seconds=8\nmin_interval_seconds=0\n");
    let marker = r.temp.path().join("helper-used");
    let before = r.git(&["rev-parse", "HEAD"]);
    r.write(".git/FETCH_HEAD", "Preserved synthetic FETCH_HEAD\n");
    let output = r
        .command()
        .env("MEMQ_FIXTURE_CREDENTIAL", &credential)
        .env("MEMQ_FIXTURE_HELPER_MARKER", &marker)
        .arg("brief")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["freshness"]["remote"]["status"], "observed");
    assert!(
        marker.exists(),
        "fetch succeeded without exercising the credential helper"
    );
    assert_eq!(r.git(&["rev-parse", "HEAD"]), before);
    assert_eq!(
        std::fs::read_to_string(r.root.join(".git/FETCH_HEAD")).unwrap(),
        "Preserved synthetic FETCH_HEAD\n"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&credential));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&credential));
}
