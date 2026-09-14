use crate::config::RemoteConfig;
use crate::error::Result;
use crate::process;
use crate::repository::Repository;
use crate::util;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::process::Command;
use std::time::Duration;

pub fn observe(
    repo: &Repository,
    access: &Connection,
    config: Option<&RemoteConfig>,
    refresh: bool,
) -> Result<Value> {
    let Some(config) = config else {
        return Ok(
            json!({"status":"unknown","observed_at":null,"ref":null,"incoming_commits":null}),
        );
    };
    let missing = |reason: &str| json!({"status":"missing_scope","observed_at":null,"ref":config.reference,"incoming_commits":null,"reason":reason});
    if config.name.is_empty()
        || config.name.starts_with('-')
        || !config.reference.starts_with("refs/heads/")
        || !repo
            .git(&["check-ref-format", &config.reference])?
            .status
            .success()
    {
        return Ok(missing("configured remote name/ref is invalid"));
    }
    let urls = repo.git(&["remote", "get-url", "--all", &config.name])?;
    if !urls.status.success()
        || urls
            .stdout
            .split(|b| *b == b'\n')
            .filter(|v| !v.is_empty())
            .count()
            != 1
    {
        return Ok(missing("configured remote is absent or ambiguous"));
    }
    let scope = util::hash_json(&json!([
        config.name,
        config.reference,
        util::hash(&urls.stdout)
    ]))?;
    let previous: Option<String> = access
        .query_row(
            "SELECT json FROM remote_observations WHERE scope=?1",
            [&scope],
            |r| r.get(0),
        )
        .optional()?;
    let mut observation:Value=previous.as_deref().map(serde_json::from_str).transpose()?.unwrap_or_else(||json!({
        "status":"unknown","observation_id":null,"observed_at":null,"ref":config.reference,"tip":null,"last_attempt":null
    }));
    let now = util::now();
    let elapsed = observation["last_attempt"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .zip(chrono::DateTime::parse_from_rfc3339(&now).ok())
        .map(|(before, after)| after.signed_duration_since(before).num_seconds());
    let due = elapsed.is_none_or(|s| s < 0 || s as u64 >= config.min_interval_seconds);
    if refresh && due {
        if unsafe_push_configuration(repo)? {
            return Ok(missing(
                "a mirror or push refspec can publish observation refs",
            ));
        }
        let destination = format!(
            "refs/memq/observe/{}/{}",
            util::escape(&config.name),
            config.reference
        );
        if !repo
            .git(&["check-ref-format", &destination])?
            .status
            .success()
        {
            return Ok(missing("cannot form a safe observation ref"));
        }
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&repo.root)
            .args([
                "fetch",
                "--no-tags",
                "--quiet",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--",
                &config.name,
                &format!("+{}:{destination}", config.reference),
            ])
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("SSH_ASKPASS_REQUIRE", "never");
        // Preserve a configured SSH command and credential helpers.
        let ssh = std::env::var("GIT_SSH_COMMAND")
            .ok()
            .or_else(|| repo.text(&["config", "--get", "core.sshCommand"]).ok());
        let variant = std::env::var("GIT_SSH_VARIANT")
            .ok()
            .or_else(|| repo.text(&["config", "--get", "ssh.variant"]).ok());
        if variant.as_deref().is_none_or(|v| v == "ssh") {
            command.env(
                "GIT_SSH_COMMAND",
                format!("{} -oBatchMode=yes", ssh.unwrap_or_else(|| "ssh".into())),
            );
        }
        let output = process::bounded(
            &mut command,
            vec![],
            Duration::from_secs(config.timeout_seconds),
            1024 * 1024,
        )?;
        observation["last_attempt"] = json!(now);
        observation["observation_id"] = json!(util::id());
        if output.success {
            let tip = repo.text(&[
                "rev-parse",
                "--verify",
                &format!("{destination}^{{commit}}"),
            ])?;
            observation["status"] = json!("observed");
            observation["tip"] = json!(tip);
            observation["observed_at"] = json!(now);
            observation["error"] = Value::Null;
        } else {
            observation["status"] = json!("failed");
            observation["error"] = json!({"reason":if output.timed_out{"timeout"}else{"fetch_failed"},"exit":output.code});
        }
        access.execute(
            "INSERT OR REPLACE INTO remote_observations VALUES(?1,?2)",
            params![scope, serde_json::to_string(&observation)?],
        )?;
    }
    observation["incoming_commits"] = if let Some(tip) = observation["tip"].as_str() {
        let head = repo.state()?.head;
        repo.text(&[
            "rev-list",
            "--count",
            &head.map_or_else(|| tip.into(), |h| format!("{h}..{tip}")),
        ])
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map_or(Value::Null, |n| json!(n))
    } else {
        Value::Null
    };
    Ok(observation)
}

fn unsafe_push_configuration(repo: &Repository) -> Result<bool> {
    let mirrors = repo.git(&["config", "--bool", "--get-regexp", r"^remote\..*\.mirror$"])?;
    if mirrors.status.success()
        && String::from_utf8_lossy(&mirrors.stdout)
            .lines()
            .any(|s| s.ends_with(" true"))
    {
        return Ok(true);
    }
    let pushes = repo.git(&["config", "--get-regexp", r"^remote\..*\.push$"])?;
    if !pushes.status.success() {
        return Ok(false);
    }
    for line in String::from_utf8_lossy(&pushes.stdout).lines() {
        let Some((_, spec)) = line.split_once(' ') else {
            continue;
        };
        let source = spec.trim_start_matches('+').split(':').next().unwrap_or("");
        if source.starts_with("refs/memq/")
            || source
                .split_once('*')
                .is_some_and(|(p, _)| "refs/memq/observe/probe".starts_with(p))
        {
            return Ok(true);
        }
    }
    Ok(false)
}
