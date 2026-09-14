#!/usr/bin/env python3
"""Measure synthetic requirement discovery; no application/provider implementation.

This deterministic evidence pass accompanies the separately recorded harness
trial. It never estimates real feature-delivery time or invents user corrections.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import subprocess
import tempfile
import time
from urllib.parse import unquote
from fixture_project import install, git, FIXTURES


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--compact", action="store_true", help="Use compact memq read replies.")
    args = parser.parse_args()
    binary = args.binary.resolve()
    presentation = ("--compact",) if args.compact else ()
    questions = [json.loads(line) for line in (FIXTURES / "facebook/questions.jsonl").read_text().splitlines()]
    with tempfile.TemporaryDirectory(prefix="memq-workflow-") as tmp:
        base = Path(tmp)
        root, data = base / "project", base / "data"
        install(root, binary, data)
        env = dict(os.environ, MEMQ_DATA_DIR=str(data))
        def run(*command):
            start = time.perf_counter()
            out = subprocess.run([str(binary), "--repo", str(root), *command],
                                 env=env, capture_output=True, text=True, timeout=60, check=True)
            return json.loads(out.stdout), (time.perf_counter() - start) * 1000, out.stdout
        def tokens(text):
            path = base / "count.txt"
            path.write_text(text)
            return run("measure", "--input", str(path))[0]["tokens"]
        def record_id(item):
            # Compact output keeps canonical mq:project:source:native identity.
            return item.get("native_id", unquote(item["id"].split(":", 3)[3]))

        # A local bare remote models a teammate acceptance; no external push,
        # checkout mutation or production credential is involved.
        remote = base / "remote.git"
        git(base, "clone", "--bare", "-q", str(root), str(remote))
        teammate = base / "teammate"
        git(base, "clone", "-q", str(remote), str(teammate))
        manifest = json.loads((teammate / "specs/manifest.json").read_text())
        for record in manifest["adrs"]:
            if record["id"] == "pending-provider-decision":
                record["status"] = "accepted"
                record["decider"] = "maintainer"
                record["reason"] = "The execution decision is accepted upstream; local provider readiness still requires evidence."
        (teammate / "specs/manifest.json").write_text(json.dumps(manifest, indent=2, ensure_ascii=False))
        git(teammate, "add", "specs/manifest.json")
        git(teammate, "commit", "-q", "--no-gpg-sign", "-m", "Synthetic authorized acceptance")
        git(remote, "fetch", "-q", str(teammate), "HEAD:refs/heads/main")
        git(root, "remote", "add", "origin", str(remote))
        config_path = root / ".memq/config.toml"
        config_path.write_text(config_path.read_text() + '\n[remote]\nname="origin"\nref="refs/heads/main"\nmin_interval_seconds=300\ntimeout_seconds=5\n')
        head = git(root, "rev-parse", "HEAD")
        upstream = git(remote, "rev-parse", "refs/heads/main")
        first, first_ms, first_wire = run("brief", "--budget", "4000", *presentation)
        warm, warm_ms, _ = run("brief", "--budget", "4000", *presentation)
        if tokens(first_wire) != first["budget"]["used"]:
            raise RuntimeError("independent brief-payload accounting mismatch")
        _, rebuild_ms, _ = run("rebuild")
        db_size = sum(p.stat().st_size for p in data.rglob("*") if p.is_file())
        baseline_text = (root / "specs/manifest.json").read_text()
        baseline_text += "\n" + (teammate / "specs/manifest.json").read_text()
        baseline_tokens = tokens(baseline_text)
        baseline_records = json.loads((root / "specs/manifest.json").read_text())
        baseline_ids = {r["id"] for key in ("next","specs","adrs","plans","reviews") for r in baseline_records[key]}
        rows = []
        for question in questions:
            found = set()
            returns, latency = 0, 0
            path_text = ""
            observations = []
            shown_ids = set()
            for query in question["queries"]:
                result, elapsed, wire = run("search", query, "--incoming", "true", "--budget", "16000", *presentation)
                latency += elapsed
                # Count stdout directly, including the complete response envelope.
                count = tokens(wire)
                if count != result["budget"]["used"]:
                    raise RuntimeError("independent full-payload accounting mismatch")
                returns += count
                found.update(record_id(i) for i in result["items"])
                for item in result["items"]:
                    if record_id(item) in question["expected_record_ids"] and item["id"] not in shown_ids:
                        shown_ids.add(item["id"])
                        command = ("show", item["id"], "--incoming", "true",
                                   "--view-id", result["freshness"]["view_id"], "--budget", "16000", *presentation)
                        continuation = None
                        while True:
                            page = ("--continuation", continuation) if continuation else ()
                            shown, elapsed, wire = run(*command, *page)
                            latency += elapsed
                            count = tokens(wire)
                            if count != shown["budget"]["used"]:
                                raise RuntimeError("independent show-payload accounting mismatch")
                            returns += count
                            observations.extend(shown["items"])
                            continuation = shown["continuation"]
                            if not continuation:
                                break
            for path in question["expected_source_paths"]:
                start = time.perf_counter()
                path_text += (root / path).read_text()
                latency += (time.perf_counter() - start) * 1000
            expected = set(question["expected_record_ids"])
            incoming = [i for i in observations if record_id(i) == "pending-provider-decision" and i["observation"]["origin"] == "incoming"]
            rows.append({
                "id": question["id"], "required_records": len(expected),
                "baseline_records_found": len(expected & baseline_ids),
                "memq_records_found": len(expected & found),
                "missing_records": sorted(expected - found),
                "required_current_files_inspected": len(question["expected_source_paths"]),
                "baseline_manifest_tokens": baseline_tokens,
                "memq_search_show_tokens": returns,
                "common_current_file_tokens": tokens(path_text),
                "memq_ms": round(latency, 3),
                "scripted_retrieval_queries": len(question["queries"]),
                "correction_prompts": None,
                "upstream_accepted_observed": any(i["acceptance"] == "accepted" for i in incoming) if question["id"] == "Q6" else None,
                "forbidden_claims": question["unsupported_claims"],
            })
        if any(row["missing_records"] for row in rows):
            raise RuntimeError("workflow required evidence missing")
        if not rows[5]["upstream_accepted_observed"]:
            raise RuntimeError("workflow incoming acceptance not observed")
        if git(root, "rev-parse", "HEAD") != head:
            raise RuntimeError("observation moved local HEAD")
        rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
        result = {
            "format":1,"experiment":"synthetic-workflow-evidence-pass",
            "representation":"compact" if args.compact else "full",
            "fixture_basis":"Synthetic login project with decisions, blockers, reviews, and sample code.",
            "question_count":len(rows), "requirement_record_coverage":1.0,
            "source_model":"synthetic five-registry project; current files inspected identically in both variants",
            "first_brief_ms":round(first_ms,3),"repeated_brief_ms":round(warm_ms,3),"full_rebuild_ms":round(rebuild_ms,3),
            "same_view_reused":first["freshness"]["view_id"]==warm["freshness"]["view_id"],
            "first_brief_tokens":first["budget"]["used"],"first_brief_incomplete":first["incomplete"],
            "retained_store_bytes_after_rebuild":db_size,
            "maximum_child_rss_bytes":rss if platform.system()=="Darwin" else rss*1024,
            "local_head_preserved":True,"incoming_revision_distinct":upstream!=head,
            "questions":rows,
            "limitations":[
                "Deterministic retrieval/evidence pass; actual harness answer inspection is reported separately.",
                "Correction prompts and unsupported model claims cannot be inferred from this script.",
                "Baseline reads both local and observed upstream manifests in full; shared current-file tokens are reported separately.",
                "No Facebook implementation, provider configuration, live login, device verification or real feature-delivery time is measured.",
                "First access is process cold with warm OS caches; this does not claim cold-disk latency.",
                "All prototype parameters remain provisional."
            ]
        }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, indent=2, ensure_ascii=False)+"\n")
    print(json.dumps({k:result[k] for k in ("question_count","requirement_record_coverage","first_brief_ms","repeated_brief_ms","full_rebuild_ms","same_view_reused")}))


if __name__ == "__main__":
    main()
