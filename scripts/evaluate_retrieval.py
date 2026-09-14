#!/usr/bin/env python3
"""Evaluate real embeddings through memq/USearch against lexical retrieval.

Judgments are fixed in tests/fixtures/retrieval/cases.jsonl. This never treats a
deterministic fixture embedder as semantic evidence. Outputs omit runtime paths.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import tempfile
import time
from fixture_project import git, FIXTURES

MODEL = "intfloat/multilingual-e5-small@614241f622f53c4eeff9890bdc4f31cfecc418b3"


def run(binary, root, data, *args):
    start = time.perf_counter()
    out = subprocess.run([str(binary), "--repo", str(root), *args],
                         env=dict(os.environ, MEMQ_DATA_DIR=str(data)),
                         check=True, capture_output=True, text=True, timeout=120)
    return json.loads(out.stdout), (time.perf_counter() - start) * 1000


def summary(rows):
    result = {}
    for split in ("dev", "heldout"):
        for language in ("all", "ar", "en"):
            selected = [r for r in rows if r["split"] == split and (language == "all" or r["language"] == language)]
            if not selected:
                continue
            result[f"{split}/{language}"] = {
                "questions": len(selected),
                "recall_at_5": statistics.mean(r["recall_at_5"] for r in selected),
                "mrr_at_10": statistics.mean(r["reciprocal_rank_at_10"] for r in selected),
                "latency_ms_median": statistics.median(r["latency_ms"] for r in selected),
                "latency_ms_max": max(r["latency_ms"] for r in selected),
            }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--embedding-url", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    adapter = Path(__file__).resolve().parents[1] / "examples" / "embed-local.py"
    cases = [json.loads(line) for line in (FIXTURES / "retrieval" / "cases.jsonl").read_text().splitlines()]
    results = {}
    with tempfile.TemporaryDirectory(prefix="memq-real-retrieval-") as temp:
        for mode in ("lexical", "hybrid"):
            root = Path(temp) / mode
            root.mkdir()
            data = Path(temp) / (mode + "-data")
            git(root, "init", "-q", "-b", "main")
            (root / "seed").write_text("Synthetic retrieval evaluation\n")
            git(root, "add", "--all")
            git(root, "commit", "-q", "--no-gpg-sign", "-m", "Synthetic corpus")
            run(binary, root, data, "init")
            config = (root / ".memq" / "config.toml").read_text()
            if mode == "hybrid":
                config += '\n[vectors]\ncommand = ' + json.dumps(["python3", str(adapter), "--url", args.embedding_url]) + '\n'
                config += f'model = "{MODEL}"\ndims = 384\npreprocessing_version = "e5-prefix-v1"\ntimeout_seconds = 60\n'
            config += '''
[[source]]
id = "memory"
kind = "json-records"
path = "records.json"
collection = "records"
id_field = "id"
references = ["file"]
[source.policy]
status_field = "status"
accepted_values = ["accepted"]
proposed_values = ["proposed"]
supersedes_field = "supersedes"
decider_field = "decider"
approvers = ["owner"]
require_attribution = true
'''
            (root / ".memq" / "config.toml").write_text(config)
            records = json.loads((FIXTURES / "retrieval" / "records.json").read_text())
            (root / "records.json").write_text(json.dumps(records, ensure_ascii=False))
            run(binary, root, data, "reconcile")
            embed_time = 0
            embedding = None
            if mode == "hybrid":
                embedding, embed_time = run(binary, root, data, "embed")
                if embedding["published"] != len(records["records"]):
                    raise RuntimeError("real model did not publish the expected corpus")
            rows = []
            for case in cases:
                if "edit" in case:
                    for record in records["records"]:
                        if record["id"] == case["edit"]["id"]:
                            record["text"] = case["edit"]["text"]
                    (root / "records.json").write_text(json.dumps(records, ensure_ascii=False))
                    if mode == "hybrid":
                        run(binary, root, data, "embed")
                result, latency = run(binary, root, data, "search", case["query"], "--budget", "30000")
                ids = [i["native_id"] for i in result["items"]]
                expected = set(case["expected"])
                recall = len(expected.intersection(ids[:5])) / len(expected)
                reciprocal = next((1 / (rank + 1) for rank, native in enumerate(ids[:10]) if native in expected), 0)
                if mode == "hybrid" and result["coverage"]["vectors"] != "ready":
                    raise RuntimeError("real vector coverage is not ready")
                rows.append({
                    "id": case["id"], "split": case["split"], "language": case["language"],
                    "kind": case["kind"], "expected": case["expected"], "top_10": ids[:10],
                    "recall_at_5": recall, "reciprocal_rank_at_10": reciprocal,
                    "latency_ms": round(latency, 3), "returned_tokens": result["budget"]["used"],
                    "query_relaxed": result["coverage"]["query_relaxed"],
                    "expected_vector_only": [i["native_id"] for i in result["items"][:5]
                        if i["native_id"] in expected and i["paths_found"] == ["vector"]],
                    "freshness": result["freshness"]["status"],
                })
            _, warm = run(binary, root, data, "search", cases[-1]["query"], "--budget", "30000")
            results[mode] = {"summary": summary(rows), "questions": rows,
                             "embed_ms": round(embed_time, 3), "embedding": embedding,
                             "repeated_query_ms": round(warm, 3)}
    rss = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    # Darwin reports bytes, Linux reports KiB. The persistent model is measured
    # separately; this figure is the maximum child-process RSS, not a sum.
    rss_bytes = rss if platform.system() == "Darwin" else rss * 1024
    report = {
        "format": 1, "experiment": "real-semantic-retrieval",
        "model": MODEL, "dimensions": 384, "preprocessing": "e5-prefix-v1",
        "corpus_records": 32, "fusion_k": 60, "candidate_limit": 50,
        "parameters": "provisional; no default model is adopted",
        "judgments": "synthetic, authored before running; separate development and held-out questions",
        "hardware": {"os": platform.system(), "architecture": platform.machine()},
        "maximum_cli_child_rss_bytes": rss_bytes,
        "limitations": ["Small synthetic corpus; not production recall.", "Held-out questions were not used to tune these parameters.",
                       "Model process RSS is reported separately.", "Latency includes the CLI, reconciliation and query embedding.",
                       "A warm repeated query may reuse the immutable result set."],
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps({mode: results[mode]["summary"] for mode in results}, ensure_ascii=False))


if __name__ == "__main__":
    main()
