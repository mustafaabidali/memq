#!/usr/bin/env python3
"""Install a synthetic Git project for local checks. Never alters an existing repo."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess

FIXTURES = Path(__file__).resolve().parents[1] / "tests" / "fixtures"


def git(root, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="Fixture", GIT_COMMITTER_NAME="Fixture",
               GIT_AUTHOR_EMAIL="fixture@example.invalid", GIT_COMMITTER_EMAIL="fixture@example.invalid",
               GIT_AUTHOR_DATE="2026-09-13T18:00:00Z", GIT_COMMITTER_DATE="2026-09-13T18:00:00Z")
    return subprocess.run(["git", "-C", str(root), *args], env=env, check=True,
                          capture_output=True, text=True).stdout.strip()


def install(root, binary, data):
    root = Path(root).resolve()
    if root.exists():
        raise ValueError("fixture destination must not already exist")
    root.mkdir(parents=True)
    for name in ("apps", "specs", "docs"):
        shutil.copytree(FIXTURES / "facebook" / name, root / name)
    git(root, "init", "-q", "-b", "main")
    git(root, "add", "--all")
    git(root, "commit", "-q", "--no-gpg-sign", "-m", "Synthetic requirement fixture")
    env = dict(os.environ, MEMQ_DATA_DIR=str(Path(data).resolve()))
    result = subprocess.run([str(binary), "--repo", str(root), "init"],
                            env=env, check=True, capture_output=True, text=True)
    project = json.loads(result.stdout)["project_id"]
    config = (FIXTURES / "config" / "valid-project.toml").read_text()
    config = config.replace("01ARZ3NDEKTSV4RRFFQ69G5FAV", project)
    (root / ".memq" / "config.toml").write_text(config)
    git(root, "add", "--all")
    git(root, "commit", "-q", "--no-gpg-sign", "-m", "Synthetic memq configuration")
    return project


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", required=True, type=Path)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--data", required=True, type=Path)
    args = parser.parse_args()
    project = install(args.destination, args.binary.resolve(), args.data)
    print(json.dumps({"fixture": "installed", "project_id": project}))
